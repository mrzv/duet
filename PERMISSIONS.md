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
- Applied Unix modes are masked to permission/special bits before `chmod`, so
  file-type bits from `symlink_metadata` are not passed back to the OS.

## Remaining Work

### 1. Apply Is Still Not Transactional

Current preflight catches common permission failures before mutation, but sync is
not a true transaction. Local and remote apply can still mutate files before a
later non-preflighted error, crash, or race is detected.

Remaining work:

- Add a prepare/commit protocol for both sides.
- Stage file writes and metadata changes before commit.
- Define rollback or resumable recovery for failures after partial mutation.
- Save state only after both sides have committed, with clear recovery guidance
  if state save fails after filesystem mutation.

### 2. Preflight Is Still Best-Effort

Current preflight catches common permission failures before mutation, including
source reads, destination parent/metadata checks, and local/remote state-save
paths. It is still a snapshot: the filesystem can change after preflight, and
some capabilities cannot be proven without attempting the operation.

Remaining work:

- Improve chmod and utime preflight where ownership/platform support makes that
  possible.
- Expand preflight coverage for less common replacement/remove combinations as
  they are found.
- Treat races between preflight and apply as normal errors with recovery advice.

### 3. Remote Errors Are Only Partly Structured

Remote sync errors now use a structured `RemoteSyncError` envelope inside
`RPCErrorKind::Other`. This is a practical improvement over generic strings, but
it is not a complete end-to-end error model yet.

Remaining work:

- Promote the envelope into a shared sync error type instead of an RPC-only
  wrapper.
- Preserve structured source chains without relying on formatted debug strings.
- Add client-side parsing/rendering so user-facing messages are concise while
  retaining machine-readable fields.
- Extend structured setup errors for server launch, SSH, profile, and log/state
  setup paths that fail before the normal RPC server is running.

### 4. Permission Model Is Still Unix Mode Bits Only

Duet records and syncs mode bits, but it does not sync ownership, ACLs, xattrs,
or platform-specific permission models. Symlink permissions are intentionally
ignored. When applying metadata, Duet masks the recorded mode to Unix
permission/special bits (`0o7777`) before calling `chmod`.

Remaining work:

- Document the supported metadata model in user-facing docs: Unix mode bits and
  mtimes, not ownership/ACLs/xattrs.
- Decide whether uid/gid support is in scope. If it is, gate it behind explicit
  capability checks and clear behavior for non-root users.

### 5. Server, Profile, And SSH Setup Still Need Better Diagnostics

Most permission-triggerable setup `expect`/`unwrap` paths in profile loading,
remote command expansion, RPC setup, and orchestration have been converted to
contextual errors. Some setup failures still happen before normal sync error
handling is established and can produce weak diagnostics.

Known examples:

- SSH key permission failures rely on OpenSSH output and do not provide a
  targeted `chmod 600` hint.
- Server startup failures before RPC initialization still depend on child
  process stderr/log context.

Remaining work:

- Add targeted SSH diagnostics for common key-permission failures.
- Preserve remote server startup failures with enough context to identify log,
  config, state, temp, and base-path permission problems.

### 6. Restricted Sync Can Still Enumerate Readable Ancestors Broadly

Restricted sync now benefits from propagated scanner errors, so inaccessible
ancestors fail safely. It has not been optimized to walk only the direct path
components needed for the restricted subtree.

Remaining work:

- Optimize restricted scans to avoid broad ancestor enumeration where possible.
- Keep path-aware failure messages for any ancestor that must still be read or
  statted.

### 7. There Is No Permission-Denied Skip Or Per-File Recovery Policy

The current policy is fail-fast. That is safer than silently skipping unreadable
paths, but it means one inaccessible path aborts the sync.

Remaining work:

- Decide whether an explicit skip policy is desirable.
- If implemented, communicate skipped permission-denied paths to both sides so
  they are not interpreted as removals or legitimate updates.
- Make skipped-path behavior opt-in and visible in the action plan.

## Current Priority Order

1. Design a transactional or resumable apply protocol.
2. Add recovery advice for races and post-preflight failures.
3. Promote `RemoteSyncError` into an end-to-end structured error model with
   client-side rendering.
4. Add targeted SSH/server-startup diagnostics.
5. Document the user-facing metadata model outside this internal status file.
6. Decide whether permission-denied skip/per-file recovery is in scope.
