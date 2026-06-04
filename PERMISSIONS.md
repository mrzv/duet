# Permission Handling Status

This document tracks the remaining permission-handling work in Duet. Older
audit documents have been consolidated here so there is a single source of
truth.

Duet has two separate permission concerns:

- It synchronizes Unix mode bits for files and directories as metadata.
- It depends on OS permissions to scan, read, write, remove, chmod, set mtimes,
  launch the server, and save state.

The highest-risk bugs around permission failures being mistaken for deletions
have been fixed. The remaining work is mostly hardening, transactionality,
structured diagnostics, and documenting the exact metadata model.

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
  changes.
- Already-synced read-only destination directories can be temporarily made
  writable for child file updates and then restored.
- Remote RPC handlers preserve more underlying error context than the old
  generic messages.
- Streamed apply temp files use bounded-length names, so long destination file
  names no longer create overlong temporary path components.

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

### 2. Preflight Coverage Is Incomplete

The current preflight focuses on destination parent write failures caused by a
directory being made read-only, plus support for temporarily chmodding existing
read-only destination directories. It does not prove every later operation will
succeed.

Remaining work:

- Preflight source readability for all files that will be checksummed,
  signatured, or transferred.
- Preflight destination remove/create/rename capability for all affected paths.
- Preflight chmod and utime capability for metadata-only changes.
- Preflight local and remote state-file writability before filesystem mutation.
- Treat races between preflight and apply as normal errors with recovery advice.

### 3. Remote Errors Are Not Yet Structured

Remote errors now include more context, but they are still serialized mostly as
strings inside `RPCErrorKind::Other`. They do not consistently expose side,
operation, path, OS error kind, and source chain as structured data.

Remaining work:

- Define a serializable sync error type with fields for side, operation, path,
  OS error kind, and message/source chain.
- Use that type across scan, state, transfer, apply, and server setup paths.
- Preserve structured error details through streamed detail/apply RPCs.
- Render concise user-facing messages from the structured errors.

### 4. Permission Model Is Still Unix Mode Bits Only

Duet records and syncs mode bits, but it does not sync ownership, ACLs, xattrs,
or platform-specific permission models. Symlink permissions are intentionally
ignored.

Remaining work:

- Document the supported metadata model in user-facing docs: Unix mode bits and
  mtimes, not ownership/ACLs/xattrs.
- Mask modes to permission bits when calling `chmod` instead of applying the
  full metadata mode from `symlink_metadata`.
- Decide whether uid/gid support is in scope. If it is, gate it behind explicit
  capability checks and clear behavior for non-root users.

### 5. Server, Profile, And SSH Setup Still Have Panic Or Weak-Diagnostic Paths

Some setup paths can still panic or produce generic errors before normal sync
error handling is established.

Known examples:

- `src/profile.rs` unwraps `shellexpand::full("~/.config/duet/")`.
- `src/remote.rs` still uses `expect("Failed to expand command")` for local
  server command expansion.
- `src/orchestrator.rs` still uses `expect` around some remote RPC setup and
  transfer calls such as `set_base`, `set_actions`, signatures, and details.
- SSH key permission failures rely on OpenSSH output and do not provide a
  targeted `chmod 600` hint.

Remaining work:

- Convert setup `unwrap`/`expect` calls to `Result` with operation/path context.
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

1. Replace remaining setup/orchestration `expect` calls that can be triggered by
   permission or environment failures.
2. Expand preflight to cover state writability, chmod/utime, source reads, and
   destination operations.
3. Add structured sync errors across RPC boundaries.
4. Document and tighten the Unix mode-bit metadata model.
5. Design a transactional or resumable apply protocol.
6. Decide whether permission-denied skip/per-file recovery is in scope.
