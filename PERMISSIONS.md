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
- Remote RPC handlers wrap sync failures in a shared `StructuredSyncError`
  envelope with side, operation, optional path, classified error kind, and source
  message. RPC paths backed by `color_eyre::Report` preserve exposed source
  chains as structured `source:` entries instead of relying only on formatted
  debug text.
- Streamed apply temp files use bounded-length names, so long destination file
  names no longer create overlong temporary path components.
- Streamed and non-streamed file-content writes now use the same side-local
  temporary-output primitive before renaming into place. Non-streamed diff apply
  restores directly into that temporary file instead of materializing the full
  updated file in memory first.
- Setup/orchestration paths that previously used permission-triggerable
  `expect`/`unwrap` calls now return contextual errors.
- SSH/session setup now adds targeted hints for common private-key and SSH config
  permission failures, and server launch errors include command/log context. SSH
  session, profile-loading, local-base expansion, remote-profile parsing, and
  server-launch failures now use the shared structured error renderer even when
  they occur before the RPC server is available. Remote server setup also uses
  the shared renderer for log path, log directory, log file, default state-dir,
  and server task startup failures.
- Applied Unix modes are masked to permission/special bits before `chmod`, so
  file-type bits from `symlink_metadata` are not passed back to the OS.
- `README.md` documents the user-facing metadata model: file contents,
  directories, symlink targets, Unix mode bits, and mtimes are synchronized;
  ownership, ACLs, xattrs, platform-specific permission models, and symlink
  permissions are not.
- Local and remote apply now create a side-local recovery marker before applying
  changes, record a shared attempt id, the apply/state-save phase, affected
  paths, operation summaries, unstaged-operation classifications, and staged
  temporary file paths, append committed-operation records as side-local actions
  complete, preserve committed-step records for file renames and direct
  remove/create/metadata operations, and remove the marker after state save
  succeeds. A later sync refuses to run if the marker remains, with phase- and
  operation-aware recovery instructions instead of silently continuing from an
  unknown partial-apply state.
- New peers prepare the remote apply marker before local mutation starts, so both
  sides have recovery markers before the concurrent apply phase begins.
- Permission tests now cover a representative race where the remote destination
  becomes unwritable after remote apply recovery is prepared; the next sync is
  blocked by the recovery marker instead of proceeding from unknown state.

## Remaining Work

### 1. Apply Needs A Prepare/Commit Protocol

Current preflight catches common permission failures before mutation, and apply
now records recovery markers while filesystem changes and state saves are in
progress. The markers include the side, base, state file, shared apply attempt
id, current phase, affected paths, compact operation summaries,
unstaged-operation classifications for direct commit operations, plus
committed-operation records for side-local actions that completed before
interruption. While apply is in progress, file content writes also record staged
temporary file paths that may need cleanup after a crash, and staged file writes
record committed-step entries after the temp file is renamed into place. Direct
remove, create, symlink, and metadata operations also record committed-step
entries after each step succeeds. New peers prepare both local and remote markers
before concurrent apply begins. This prevents a later run from silently
continuing after an interrupted apply. Sync is still not a true transaction:
local and remote apply can still mutate files before a later non-preflighted
error, crash, or race is detected.

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
  a protocol capability. The current protocol has a prepare marker RPC with a
  shared attempt id, but not a full staged prepare/commit/recover sequence.
- Move streamed and non-streamed apply through the same staged apply engine.
  File-content writes now share the same temporary-output primitive, but the full
  staged engine still needs to cover directory, symlink, metadata, and removal
  operations.
- Convert the broad unstaged-operation classifications into step-level commit
  records for directory removals, type replacements, chmod, and utime.
- Persist enough committed-operation metadata to resume automatically after a
  crash. The current marker records completed side-local action summaries, but it
  does not yet support automatic replay/rollback.
- Use staged-file records to offer safe automatic cleanup for abandoned temp
  files that were never renamed into place.
- Keep state saving after both sides have committed successfully.

### 2. Preflight And Apply Recovery Are Still Best-Effort

Current preflight catches common permission failures before mutation, including
source reads, destination parent/metadata checks, and local/remote state-save
paths. It is still a snapshot: the filesystem can change after preflight, and
some capabilities cannot be proven without attempting the operation.

When a post-preflight apply or state-save error occurs, Duet now reports recovery
advice explaining that filesystem changes may have been partially applied and
state may not have been saved. If a process exits before cleanup, the recovery
marker blocks the next sync until the user inspects the listed paths and removes
the marker. Marker recovery advice is tailored to the recorded phase and to
planned destructive, metadata, or file-content operations, and it calls out when
staged-file, unstaged-operation, committed-step, or committed-operation records
are present. This is still not a resumable apply protocol.

Remaining work:

- Improve chmod and utime preflight where ownership/platform support makes that
  possible.
- Expand preflight coverage for less common replacement/remove combinations as
  they are found.
- Add more race tests as new post-preflight failure modes are found.
- Replace broad unstaged-operation guidance with automatic step-level recovery
  once apply attempts can safely replay or roll back recorded commit points.

### 3. Sync Errors Are Only Partly Structured

Remote sync errors now use the shared `StructuredSyncError` envelope inside
`RPCErrorKind::Other`. This is a practical improvement over generic strings and
removes the earlier RPC-only error type boundary, but it is not a complete
end-to-end error model yet.

The client parses this envelope for concise remote error rendering. Local setup
paths can also use the same renderer for classified user-facing diagnostics. The
original RPC payload remains line-oriented and machine-readable. RPC and setup
paths backed by `color_eyre::Report` now preserve exposed source chains as
structured `source:` lines, though some call sites still only provide formatted
messages or non-Report error values.

Remaining work:

- Convert remaining formatted-message and non-Report call sites to typed errors
  with structured source chains where practical.
- Extend structured setup errors to remaining log/state setup paths that fail
  before the normal RPC server is running.

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
available. SSH session, profile-loading, local-base expansion,
remote-profile parsing, server-launch, and remote server log/state setup
diagnostics now use the shared structured renderer. Some setup failures still
happen before normal sync error handling is established and can produce weak
diagnostics.

Known examples:

- Server startup failures before RPC initialization still depend on child
  process stderr/log context, though their final setup error is now classified
  and rendered consistently when the child reaches Duet setup code.

Remaining work:

- Preserve non-Duet remote process failures, shell failures, and SSH transport
  failures with enough stderr/log context to distinguish remote config, temp, and
  base-path permission problems before RPC starts.

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
3. Extend `StructuredSyncError` into a full end-to-end error model with
   structured source chains and setup-error rendering.
