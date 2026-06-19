# Changelog

## Unreleased

### Fixed
- Released scanner directory concurrency permits before recursive descent to avoid hanging on deeply nested trees.
- Reported restricted scans that would cross a filesystem boundary on the way to the requested path instead of treating tracked entries as removed.
- Batched small and medium streamed file-byte frames over SSH to avoid one apply RPC per file while preserving the dedicated large-file transfer path.
- Raised the streamed file-byte chunk RPC cutoff from 64 KiB to 8 MiB based on SSH benchmarks, keeping medium payloads in batched detail frames.

### Added
- Added a public `duet recover` command to inspect unfinished apply markers and optionally clear them after manual recovery.

### Documentation
- Updated architecture notes for streamed apply RPCs, file-byte chunk routing, and staged output verification.

## 0.8.7 - 2026-06-18

### Added
- Added stable remote-state identity selection that preserves existing legacy remote state files while using stable IDs for new state.
- Added a persisted client ID fallback when the machine ID is unavailable.
- Added validation for RPC-selected remote state IDs and named profile names.

### Changed
- Hardened restricted-path normalization to resolve paths against their intended base before enforcing sync-root boundaries, including symlink-aware parent handling.
- Rejected `--profile-file` SSH remotes when the derived remote state directory would be local to the client.
- Limited non-streamed detail transfer to avoid materializing very large payloads when a peer cannot stream details.
- Documented basename-only ignore glob behavior and unsupported spaces in remote profile entries.

### Fixed
- Flushed serialized state snapshots before committing atomic state writes and kept local recovery markers until both local and remote state saves succeed.
- Rejected unknown CLI arguments, extra positionals, unsafe named profile paths, and sync-only flags on hidden maintenance commands.
- Fixed profile include/exclude parsing when markers are preceded by whitespace.
- Validated deserialized action, state, RPC, and scan paths before filesystem access.
- Created apply temporary files with randomized `create_new` names instead of truncating predictable paths.
- Validated diff signatures, delta windows, detail ordering, expected detail kinds, and staged output contents before recording synchronized state.
- Detected stale diff sources before applying deltas.
- Reported unsupported special files and filesystem-boundary crossings inside the requested sync tree instead of silently propagating removals.
- Fixed short-read handling and invalid-window errors in the rsync-style comparison and restore paths.
- Propagated `_walk` scanner errors.
- Escaped control characters in user-facing action paths and structured sync errors.
- Removed an unused `scan::self` import warning in normal builds.

### Documentation
- Refreshed README usage for profile-file, debug-info, and performance flags.
- Updated architecture notes for recovery, RPC capabilities, and remote state ID selection.

## 0.8.6 - 2026-06-18

### Added
- Added build-time version suffixes for unreleased commits and dirty working copies while keeping RPC version negotiation on the package version.

### Fixed
- Preserved append-only RPC method ordering for file-byte chunk streaming so existing essrpc method numbers remain stable.
- Clamped RPC detail stream size requests to negotiated server limits.

## 0.8.5 - 2026-06-17

### Added
- Added developer-facing sync performance profiling flags for phase timings, transfer counters, and optional JSON output.
- Added hidden `DUET_SYNC_*` environment overrides for experimenting with sync tuning values during profiling.
- Added streamed sync server-side performance telemetry to separate transport time from remote detail/apply work.
- Added a `file-byte-chunks-v1` streamed apply fast path that uses byte-optimized RPC parameters for large whole-file uploads.

### Changed
- Split streamed sync performance profile output into detail-generation and apply sub-phases for both directions.
- Lowered the preferred adaptive signature-window ceiling to 64 KiB to avoid large-window diff performance cliffs observed during profiling.
- Increased preferred detail chunk and payload sizes to 64 MiB to reduce SSH round trips for large file transfers.
- Reduced allocation overhead when streaming many small whole-file changes.
- Reused the streamed apply recovery-marker append handle to reduce per-file apply overhead.

## 0.8.0 - 2026-06-17

### Added
- Added `sync-tuning-v1` capability negotiation so newer clients and servers can agree on signature-window and detail-stream batching settings without requiring lockstep upgrades.
- Added adaptive per-file signature windows for modified files, using the square root of the file size clamped by negotiated limits.
- Added debug output for the selected sync tuning values.

### Changed
- Increased preferred detail chunk and payload sizes from 1 MiB to 4 MiB when both peers support sync tuning negotiation.
- Made signature and delta application use the window size carried by the received signature or delta instead of assuming the local legacy window size.
- Kept legacy sync tuning as the fallback for older peers that do not advertise `sync-tuning-v1`.

### Development
- Ignored local `.opencode` configuration in version control.

## 0.7.1 - 2026-06-16

### Added
- Created missing parent directories when syncing added files to a peer that advertises support for creatable added parents.
- Added `creatable-added-parents-v1` capability gating so newer clients can avoid unsafe behavior with older servers.

### Fixed
- Prevented non-empty directory removals from being applied without preflight checks for untracked children.
- Improved unfinished apply recovery messages so interrupted syncs are easier to inspect and resolve.

### Development
- Cleaned up hardening plan documentation.

## 0.7.0 - 2026-06-04

### Added
- Added phase-specific apply recovery markers that record interrupted apply attempts.
- Added operation summaries, committed-operation records, committed-step records, staged-file records, and correlation IDs for recovery markers.
- Added an apply recovery inspection command.
- Added tests for post-preflight remote permission races.

### Changed
- Hardened change application by unifying file content staging and recording both staged and direct apply commit steps.
- Tightened Unix mode handling to synchronize permission and special bits without treating file-type bits as normal mode metadata.
- Improved apply recovery advice with more specific guidance about removed paths, metadata operations, and file content changes.

### Fixed
- Replaced setup panic paths with structured errors.
- Expanded permission preflight checks before applying changes.
- Preserved structured setup and RPC error source chains.
- Rendered local profile setup errors, remote server setup errors, and general setup errors consistently through the sync error model.

### Documentation
- Added and revised permission-handling documentation, including metadata model and skip policy notes.
- Updated architecture documentation and consolidated permissions follow-up docs.

## 0.6.0 - 2026-06-03

### Added
- Added permission behavior tests and enabled permission stress tests.
- Added documentation analyzing permission-handling problems and tradeoffs.

### Fixed
- Propagated scan permission errors instead of treating unreadable paths as ordinary deletions or updates.
- Reported local and remote state file permission errors with better context.
- Preserved remote permission error context across RPC boundaries.
- Returned apply permission errors and preflighted readonly apply conflicts.
- Shortened streamed apply temporary filenames to avoid path length problems.

## 0.5.0 - 2026-06-03

### Added
- Added profile-file sync support.
- Added local sync integration tests and regression tests around refactor seams.
- Added remote protocol negotiation and made remote protocol negotiation run before sync behavior depends on server capabilities.
- Added streamed detailed change transfer to avoid materializing all detailed changes before applying them.
- Added batching for streamed detail frames and progress reporting for streamed syncs.
- Added a sync debug information flag that reports protocol and capability negotiation details.

### Changed
- Refactored the command entry point into focused modules.
- Split sync orchestration into explicit phases.
- Propagated errors through straightforward refactor paths instead of hiding them behind older control flow.

### Fixed
- Fixed tab navigation behavior when there are no conflicts.

### Documentation
- Added architecture documentation.

## 0.3.2 - 2026-05-03

### Changed
- Shortened hexadecimal components in path display.

### Dependencies
- Updated `bytes` from 1.10.1 to 1.11.1.
- Updated `rand` from 0.8.5 to 0.8.6.
- Updated `tracing-subscriber` from 0.3.19 to 0.3.20.

## 0.3.1 - 2025-09-27

### Added
- Added an interactive-mode hint about using Tab and Shift-Tab.
