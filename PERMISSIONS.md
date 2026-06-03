# Permission Handling Review

This document reviews how Duet currently behaves when filesystem permissions
prevent access to files, directories, metadata, state files, or remote-side
resources. It also lists the fixes that would make the sync fail safely.

The central problem is that permission failures are not handled consistently.
Some failures propagate as errors, some panic, and the most dangerous scan
failures can be swallowed and interpreted as legitimate deletions.

## Current Behavior

Duet has two distinct permission concerns:

- It synchronizes Unix mode bits for files and directories as metadata.
- It depends on OS permissions to scan, read, write, remove, chmod, set mtimes,
  launch the server, and save state.

Those concerns are currently mixed together in a way that can produce unsafe
results.

## Issues And Fixes

### 1. Unreadable scan roots or subdirectories can look like deletions

`scan_entries` spawns `scan::scan` but never awaits the task result. If the
scanner panics, the channel closes and `scan_entries` returns whatever entries
were already sent, possibly an empty scan.

Relevant code:

- `src/main.rs`: `scan_entries` spawns and ignores the scan task result.
- `src/scan/mod.rs`: scanner uses `expect` for `read_dir`, `next_entry`, and
  `symlink_metadata`.

Consequences:

- If the local side cannot read a directory, old local entries may appear
  removed and Duet may delete them on the remote side.
- If the remote side cannot read a directory, old remote entries may appear
  removed and Duet may delete them locally.
- If both sides cannot read the same subtree, Duet may save an empty or partial
  state and lose tracking, even if the files still exist.

Fix:

- Make scanning return `Result<Vec<Entry>>`.
- Await the scan task and propagate `JoinError` and scan errors.
- Treat `PermissionDenied`, inaccessible metadata, and traversal failures as
  fatal by default.
- Never compute changes from a partial scan unless the user explicitly opted
  into a skip policy for specific ignored paths.

### 2. Scanner filesystem failures use `expect` and `unwrap`

The scanner panics for base metadata, directory reads, directory-entry reads,
entry metadata, and some setup failures.

Relevant code:

- `src/scan/mod.rs`: `base.symlink_metadata().ok().unwrap()`
- `src/scan/mod.rs`: `fs::read_dir(...).await.expect(...)`
- `src/scan/mod.rs`: `dir.next_entry().await.expect(...)`
- `src/scan/mod.rs`: `fs::symlink_metadata(...).await.expect(...)`

Fix:

- Replace these with `?` and path-aware context.
- Include side, operation, and path in errors, for example:
  `local: cannot read directory /path/to/dir: Permission denied`.
- Return the error to the sync driver and remote RPC layer.

### 3. Unreadable changed files crash during checksum or content collection

Unchanged files are usually only scanned via metadata. Added or modified files
are later opened to compute checksums, signatures, or detailed changes. If that
open/read fails, the app often panics or reports a generic error.

Relevant code:

- `src/main.rs`: checksum computation calls `compute_checksum(...).expect(...)`.
- `src/sync.rs`: signatures and detailed changes open/read source files.
- `src/main.rs`: callers often use `expect` on these results.

Consequences:

- A file can be visible in directory metadata but unreadable as content.
- Dry-run can still fail because checksums are computed before display.
- Remote-side read failures lose path details or crash the server task.

Fix:

- Propagate checksum, signature, and detail-read errors with path context.
- Abort before showing or applying a plan if any changed source file is
  unreadable.
- Optionally add an explicit future policy for ignored or skipped inaccessible
  files, but keep abort as the default.

### 4. Apply-side permission failures can leave partial filesystem changes

Applying changes uses many direct filesystem operations: remove file, remove
directory, create directory, create symlink, atomic file replacement, chmod, and
mtime update. Many of these use `expect`, so failures panic after earlier
actions may already have changed the filesystem.

Relevant code:

- `src/sync.rs`: `remove_file(...).expect(...)`
- `src/sync.rs`: `create_dir(...).expect(...)`
- `src/sync.rs`: `symlink(...).expect(...)`
- `src/sync.rs`: `remove_dir(...).expect(...)`
- `src/sync.rs`: `set_permissions(...).expect(...)`
- `src/sync.rs`: `set_symlink_file_times(...).expect(...)`

Consequences:

- A destination parent without write permission can fail after other changes
  were applied.
- A chmod or mtime failure can happen after file contents were already replaced.
- The state usually is not saved after a panic, leaving filesystem and state
  out of sync.

Fix:

- Convert every apply operation to `Result` with operation and path context.
- Add a preflight phase for destination parent write/search permission, source
  readability, chmod capability, utime capability, and state-file writability.
- Longer term, stage file writes and directory changes so commits can be made
  in a controlled order with a resumable transaction log.

### 5. Local and remote apply concurrently, so one side can mutate despite the other failing

Local and remote apply run concurrently. If one side lacks permission while the
other side succeeds, the successful side may already be changed before the
failure is observed.

Relevant code:

- `src/main.rs`: local and remote apply are joined concurrently.

Consequences:

- Local write failure can still leave remote files changed.
- Remote write failure can still leave local files changed.
- State saving may be skipped, or only one side may save state later.

Fix:

- Add a two-sided preflight before either side commits.
- Prefer a two-phase protocol:
  1. prepare and stage all needed writes on both sides;
  2. commit on both sides;
  3. save state on both sides.
- If full transactional behavior is too large for now, at least fail before
  apply when either side lacks required permissions.

### 6. State-file permission handling is unsafe

State loading uses `exists()` and then `unwrap`. `exists()` can hide metadata
errors by returning false. State saving is worse locally: the result of the
atomic write is ignored.

Relevant code:

- `src/main.rs`: state read uses `if f.exists()` then `File::open(...).unwrap()`.
- `src/main.rs`: local state save ignores the inner result from `AtomicFile`.
- `src/main.rs`: remote state save maps failures to generic RPC errors.

Consequences:

- An inaccessible existing state file can be treated as missing state.
- A local state save failure can be silently ignored.
- Local and remote state can diverge after a successful filesystem sync.
- Future runs may replay changes, create conflicts, or produce misleading
  plans.

Fix:

- Use `try_exists` instead of `exists`.
- Return errors for state open, read, decode, write, and rename failures.
- Never ignore the atomic-write result.
- If filesystem changes were applied but state save failed, print a clear
  recovery message: data changed, state not saved, rerun after fixing the
  state-file permission problem.

### 7. Remote permission errors lose detail

Remote RPC handlers often collapse errors into generic messages such as
`error in getting changes from the server`. Panics may become transport EOFs.

Relevant code:

- `src/main.rs`: `DuetServerImpl::changes`
- `src/main.rs`: `DuetServerImpl::get_signatures`
- `src/main.rs`: `DuetServerImpl::get_detailed_changes`
- `src/main.rs`: `DuetServerImpl::apply_detailed_changes`
- `src/main.rs`: `DuetServerImpl::save_state`

Fix:

- Define a serializable sync error type containing:
  - side: local or remote;
  - operation: scan, read, write, delete, chmod, utime, save-state, etc.;
  - path;
  - OS error kind and message.
- Return that over RPC instead of a generic `RPCErrorKind::Other` string.

### 8. Directory modes can block future syncs

Duet applies directory metadata after child updates, which is the right final
ordering. However, an existing destination directory may already be non-writable
from a previous sync. Even if the current user owns the directory, child
updates or deletes inside it will fail unless the directory is temporarily made
writable.

Consequences:

- A directory synced as `0555` can prevent future modifications inside that
  directory.
- This failure can occur even when Duet could have temporarily chmodded the
  directory and restored the final mode afterward.

Fix:

- During preflight, identify destination directories that need child changes.
- If owned by the current user and missing required write/search bits,
  temporarily add owner write/search permission.
- Restore the intended final mode after all child operations complete.
- If temporary chmod is not allowed, abort before applying anything.

### 9. Mode-bit sync is not a complete permissions model

Duet records mode bits and has a `TODO` for uid/gid. It does not sync ownership,
ACLs, xattrs, or platform-specific permission models. It intentionally ignores
symlink permissions.

Relevant code:

- `src/scan/mod.rs`: metadata contains `mode` but not uid/gid.
- `src/actions.rs`: symlink display says permissions do not matter.
- `src/sync.rs`: `update_meta` applies mode and mtime.

Fix:

- Document the supported permission model: Unix mode bits only.
- Mask modes to permission bits when chmodding, rather than passing full
  metadata mode bits.
- Consider optional owner/group/ACL sync only with explicit capability checks
  and clear fallback behavior for non-root users.

### 10. Symlink target read failures are silently misclassified

The scanner uses `read_link(...).map_or(None, ...)`. If `read_link` fails on a
symlink, the target becomes `None`, which makes the entry look like a regular
file according to `is_symlink`.

Relevant code:

- `src/scan/mod.rs`: `target: fs::read_link(path).await.map_or(None, |p| Some(p))`
- `src/scan/mod.rs`: `is_symlink` checks whether `target` is `Some`.

Fix:

- Use `metadata.file_type().is_symlink()` to decide whether the entry is a
  symlink.
- If the entry is a symlink and `read_link` fails, return a path-aware error.

### 11. Restricted sync still depends on readable ancestors

Restricted sync avoids descending into unrelated subtrees, but it still needs to
read and stat ancestor directories on the path to the restricted subtree.
Permission failures there currently hit the same unsafe scan behavior.

Fix:

- Once scanner errors are propagated, restricted sync should fail with a clear
  path and operation.
- Where possible, optimize restricted scans to walk direct path components
  instead of enumerating broad ancestor directories.

### 12. Server setup and support files have permission failure paths

The server creates `~/.config/duet`, opens `remote.log`, and saves remote state.
The client also reads machine identity and may create SSH control sockets under
the temp directory.

Relevant code:

- `src/main.rs`: `server`
- `src/main.rs`: `local_id`
- `src/main.rs`: SSH `control_directory(std::env::temp_dir())`

Fix:

- Return startup errors to the client in a structured way where possible.
- Add context for config directory, log file, machine ID, temp directory, and
  remote state path failures.

## Recommended Fix Order

1. Make scanner errors impossible to ignore.
2. Fix state loading and saving so permission errors are never treated as
   missing state or ignored writes.
3. Convert apply operations from panics to path-aware `Result`s.
4. Add two-sided preflight before apply.
5. Improve remote error serialization.
6. Add temporary directory chmod support for owned directories that need child
   changes.
7. Document the exact permissions model and unsupported metadata.

The first two items are the most important. They eliminate the class of bugs
where permission denial can be mistaken for file deletion.
