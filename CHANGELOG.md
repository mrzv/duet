# Changelog

## Unreleased

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
