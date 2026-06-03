# Permission & Access Issue Analysis

All permission and file-access failure modes in Duet, organized by execution phase. Each issue notes whether it can occur on the **local** side, **remote** side, or **both**.

---

## Profile Parsing

| # | Issue | Side | Location | Severity |
|---|-------|------|----------|----------|
| 1 | `shellexpand::full(&cmd).expect(...)` panics if `$HOME` is unset or the command string can't be expanded | Local | `main.rs:492` | High |
| 2 | `File::open(profile)` returns `io::Error` via `?` — handled properly | Both | `profile.rs:47` | OK |

## Phase 1: Scanner

| # | Issue | Side | Location | Severity |
|---|-------|------|----------|----------|
| 3 | `fs::read_dir(path).await.expect("Couldn't read the directory")` — panics if a directory in the sync tree has no `r-x` permission for the user | Both | `scan/mod.rs:220` | **Critical** |
| 4 | `dir.next_entry().await.expect(...)` — panics if a directory entry is unreadable (race deletion, permission error on a single entry) | Both | `scan/mod.rs:221` | **Critical** |
| 5 | `fs::symlink_metadata(&path).await.expect(...)` — panics if metadata is inaccessible (file deleted by race, parent dir lacks `x` permission) | Both | `scan/mod.rs:229` | **Critical** |
| 6 | `base.symlink_metadata().ok().unwrap()` — panics if the sync root itself is not stat-able | Both | `scan/mod.rs:300` | High |
| 7 | `fs::read_link(path).await.map_or(None, Some)` — **silently swallows all errors** (including permission denied). An unreadable symlink target is treated as a regular file, causing the other side to create a regular file instead of a symlink | Both | `scan/mod.rs:252` | **Medium** |
| 8 | `compute_checksum()` uses `?` to propagate `io::Error` — the caller wraps it in `.expect(...)`, so this still panics | Both | `scan/mod.rs:118` → `main.rs:716-717` | High |

## Phase 2: State File Loading

| # | Issue | Side | Location | Severity |
|---|-------|------|----------|----------|
| 9 | `tokio::fs::File::open(f).await.unwrap()` — panics if state file exists but is unreadable (wrong permissions, missing directory, etc.) | Both | `main.rs:679` | **Critical** |
| 10 | `f.read_to_end(&mut contents).await.unwrap()` — panics on I/O error during read | Both | `main.rs:681` | High |
| 11 | `deserialize_from(...).unwrap()` — panics if state file is corrupted or truncated | Both | `main.rs:683` | High |

## Phase 3–4: Signature & Content Collection

| # | Issue | Side | Location | Severity |
|---|-------|------|----------|----------|
| 12 | `fs::File::open(base.join(e1.path()))?` in `get_signatures()` — returns `io::Error` (wrapped by `?` then by RPC `match Err(_)`). The old file must be readable to build a signature | Both | `sync.rs:28` → `main.rs:601-605` | Medium |
| 13 | `fs::read()`, `fs::File::open()` in `get_detailed_changes()` — source-side reads of the new file can fail if file became unreadable between scan and transfer | Both | `sync.rs:60,67,72` → RPC wrappers | Medium |

## Phase 5: Apply (Filesystem Mutation)

| # | Issue | Side | Location | Severity |
|---|-------|------|----------|----------|
| 14 | `fs::remove_file(...).expect(...)` — panics if file can't be removed (parent dir not writable, file immutable) | Both | `sync.rs:120,161,173` | **Critical** |
| 15 | `fs::create_dir(...).expect(...)` — panics if directory can't be created | Both | `sync.rs:132,166,182` | **Critical** |
| 16 | `std::os::unix::fs::symlink(p, ...).expect(...)` — panics if symlink can't be created | Both | `sync.rs:129,163,179,248` | **Critical** |
| 17 | `fs::remove_dir(...).expect(...)` — panics if directory can't be removed (not empty, or permission denied) | Both | `sync.rs:237,246` | **Critical** |
| 18 | `create_file(...).expect(...)` → `create_file_with_contents()` — panics via `.expect()` on the outer call | Both | `sync.rs:138,176,251` → `273-280` | High |
| 19 | `update_meta()` — three panics in one function: `symlink_metadata().expect()`, `set_permissions().expect()`, `set_symlink_file_times().expect()` | Both | `sync.rs:294-302` | **Critical** |

## Phase 6: State Saving

| # | Issue | Side | Location | Severity |
|---|-------|------|----------|----------|
| 20 | `AtomicFile::write(...).expect("Failed to save local state")` — panics if state can't be written (disk full, directory read-only) | Local | `main.rs:449-455` | **Critical** |
| 21 | Server `save_state()` wraps error in `RPCError`, client does `.expect("Failed to save remote state")` — panics | Both | `main.rs:632-636` → `457` | High |

## Infrastructure

| # | Issue | Side | Location | Severity |
|---|-------|------|----------|----------|
| 22 | Server `create_dir_all("~/.config/duet")` or log file creation fails — propagates via `?`, server exits | Remote | `main.rs:645-647` | High |
| 23 | SSH key permissions too permissive — OpenSSH refuses connection. Error message is printed but doesn't tell the user to `chmod 600` | Local | `main.rs:312-323` | Low |

---

## Design-Level Issues

**24. Concurrent apply has no rollback** — `main.rs:441`: Client and server apply concurrently via `tokio::join!`. If one side fails partway through (e.g., a permission-denied panic), the other side may have already applied some or all of its changes. Snapshots are saved only *after both sides succeed*. Partial changes are on disk with no record of what was done.

**25. No per-file error recovery** — The apply loop processes actions sequentially and assumes all will succeed. Failure on entry N means entries 1..N-1 are applied, N panics, and N+1..end are never reached. No mechanism to skip, log, and continue.

**26. No permission-denied tracking** — As the TODO at `TODO:1-6` states: permission-denied files need to be tracked and communicated to the other side so those paths can be excluded from the action plan. Without this, even if the scanner skips unreadable files, the other side wouldn't know to skip them too, producing incorrect actions. A file accessible on one side might be inaccessible on the other — this asymmetry is the core challenge.

**27. Missing uid/gid tracking** — `DirEntryWithMeta` has `// TODO: uid and gid` at `scan/mod.rs:49`. Ownership is never synced, only permission bits. After sync, files on the other side will have the synced permission mode but the default uid/gid from the file creation process.

**28. Server `set_base()` home directory issue** — `main.rs:567`: `full(&base)` expands `~` via `shellexpand`. If the remote user's `$HOME` is unset (possible in restricted SSH environments), this returns an error mapped to `RPCError`. The client's `.expect("Couldn't set server base")` then panics.

---

## Classification Summary

| Severity | Issue Numbers |
|----------|---------------|
| **Critical** (crash + can leave inconsistent state) | 3, 4, 5, 9, 14, 15, 16, 17, 19, 20 |
| **High** (crash, state less ambiguous) | 1, 6, 8, 10, 11, 18, 21, 22 |
| **Medium** (incorrect behavior without crash) | 7, 12, 13 |
| **Design / architecture** | 24, 25, 26, 27, 28 |

The root cause of most issues: **every filesystem operation uses `.expect()` or `.unwrap()`**, treating any I/O error as a fatal crash rather than a recoverable condition. The TODO at `TODO:1-6` acknowledges this for the scanner; the same fix needs to extend to the apply phase and state persistence.
