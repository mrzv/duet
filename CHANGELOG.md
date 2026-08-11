# Changelog

## Unreleased

### Changed
- Replaced staged commit's action-inferred directory durability discovery with an exact identity-checked, descriptor-relative mutation and retirement ledger that records only successfully dirtied namespace parents, retires only descriptor-verified and confirmed-unlinked directories, and syncs remaining obligations deepest-first before syncing accumulated recovery records and entering the committed phase.
- Parallelized staged-output commit validation with bounded deterministic workers, reduced recovery-marker writes, skipped irrelevant directory durability discovery, and added local/remote staged commit subphase profiling.
- Reconstructed staged checkpoint manifests with a linear move-based merge and reused immutable commit-plan data across validation, mutation, and durability planning.
- Retained and identity-checked the V2 preparing-marker descriptor while staging outputs, avoiding repeated marker opens without changing content validation or durability barriers.
- Cached immutable staged-output identities and reused validated destination-parent identities, avoiding redundant metadata syscalls while preserving publication, content, metadata, recovery, and durability checks.
- Reused a single-use validated staged-output receipt to skip only the duplicate commit-start staged-file hash; commit still fully validates targets and capacity, and publication still rehashes each staged output immediately before mutation.
- Added local and remote staged-marker lifecycle profiling for prepare, state-save, finish, unlink, and marker-directory durability steps without changing synchronization behavior.
- Changed new staged recovery markers to a V3 same-inode journal with five preallocated fixed-size BLAKE2b-256 hash-chained phase slots, removing five full-marker replacements and parent-directory fsyncs per lifecycle while retaining V1/V2 recovery and fail-closed cleanup semantics; exact marker removal now quarantines before classification, cleanup, identity verification, and unlink, quarantine restoration syncs the parent, and identical V1/V3 creation retries repeat exact-content marker and parent durability barriers.

## 0.9.2 - 2026-08-09

### Added
- Added repeatable CLI-only `--exclude <path>` hard scan exclusions, including restricted-sync composition, preserved excluded baseline state, removal safety, and the append-only `scan-excludes-v1` method-47 RPC.
- Added the `staged-apply-v1` capability and append-only RPC methods for bilateral prepare, validation, commit, state save, and exact-attempt completion.
- Added V2 apply recovery markers with durable prepare/commit phases and identity-checked, retry-safe cleanup of abandoned precommit staging.
- Added subprocess SIGINT coverage for cancellation at the prepared barrier and deferred interruption after staged commit completes.
- Added `--staging-limit` and `--staging-reserve`, with a 5% per-filesystem default reserve, dry-run wave reporting, and staging capacity counters in human and JSON performance profiles.
- Added checkpointed bilateral staging waves so plans larger than one wave save clean canonical checkpoints before continuing.
- Added prepare-time APFS clone and Linux reflink support for modified regular files, preserving atomic publication while reducing physical staging to changed blocks where supported.

### Changed
- Made streamed progress identify preparation versus legacy synchronization, transfer direction, current path, staging wave, and final sealing work.
- Added post-stream staged progress for validation, commit, snapshot saving, and checkpoint finalization, and exposed validation as a separate performance phase.
- Changed supported streamed synchronization to reconstruct, verify, fsync, and seal every regular-file output in the current wave before either side mutates that wave's synchronized targets, then validate both plans before crossing a shared commit fence.
- Made the first Ctrl+C cooperatively abort before commit or defer through commit, state save, marker cleanup, and server shutdown after the fence; a second Ctrl+C still forces an immediate code-6 exit.
- Kept legacy peers compatible by retaining their existing apply paths with an earlier non-cancellable boundary.
- Switched SSH command execution to native multiplexing with a non-persistent control master and explicitly wait for local and remote server children during graceful shutdown.
- Changed Ctrl+C during an intermediate committed wave to finish that checkpoint and stop before the next wave; a deferred final-wave interrupt still finishes successfully.
- Enforced host-local staging reserves during materialized writes, after durability barriers, and immediately before commit, while retaining default legacy fallback for peers that cannot negotiate the policy.

### Fixed
- Prevented directory-removal preflight from following a tracked child symlink and misreporting the symlink target's contents as unexpected blockers.
- Kept destructive directory actions pending when unresolved descendant conflicts are skipped, preventing tracked children from being misreported as unexpected removal blockers.
- Fixed Ctrl+C only terminating the signal-handler thread while synchronization continued in the background.
- Added validation checkpoints that detect prepared output identity or content changes before the fence and immediately before publication.
- Prevented precommit cancellation from orphaning recovery markers or staging, including retries after partially completed cleanup.
- Corrected commit and state-save recovery guidance to require tree and snapshot reconciliation instead of rerunning against stale state.
- Batched staged-output recovery-record durability at sealed-output boundaries instead of syncing and reparsing the growing marker for every file, avoiding a severe many-small-file performance regression.
- Verified streamed output incrementally while writing full-file and delta data, avoiding an extra complete staged-file read while retaining commit-time validation.
- Classified native no-space and quota errors explicitly and kept reserve failures in the abortable precommit path.

## 0.9.0 - 2026-07-28

### Added
- Added BLAKE2b-256 content identity and verification, versioned V2 snapshots, append-only V2 RPC methods, and automatic scope-aware migration from legacy state.
- Added `--prune-ignored` to explicitly delete ignored files or directories that block removal of a synced parent directory.
- Added `[prune]` profile patterns for disposable ignored content that may be pruned automatically when blocking synced parent directory removal.
- Added structured local/remote preflight blocker reports for directory removal blockers.
- Added profile-aware `duet recover <profile>` and `duet recover --remote <profile>` recovery marker inspection and clearing.

### Changed
- Removed duplicated byte counts from the streamed SSH transfer progress message.
- Normalized progress display styling across scanning, content hashing, and streamed transfer.
- Interactive mode now uses Shift+Up/Shift+Down page navigation and preserves the selected row when moving a page at a time.
- Strong-capable peers now use BLAKE2b-256 for cross-side equivalence, stale file checks, and final output verification while retaining Adler-32 only for negotiated compatibility and rustsync rolling blocks.
- Snapshot maintenance now hashes complete file contents and writes the current version atomically.
- Expanded `--dry-run` to run the full non-mutating preflight checks, including local and remote directory removal blocker reports.
- Bounded directory scanning globally to 64 concurrent reads and made content hashing CPU-parallel across files, defaulting to up to one whole-file worker per available CPU (maximum 64) while preserving deterministic results.
- Reused one private staging directory in the first output parent per side-local apply phase, verified published parent identities, and batched source-stage and destination-parent durability barriers while retaining per-file output durability.
- Batched private regular-file outputs by count and content bytes, synced each batch concurrently with bounded portable workers, and published only fully synced batches in action order before recording recovery progress.
- Kept restrictive destination-parent modes unchanged while outputs are pending, limited pending descriptors using `RLIMIT_NOFILE`, and recorded each successful ordered publication before attempting the next batch item.
- Made streamed apply fail closed after any frame error, recorded file completion immediately after successful publication syscalls, validated parent identity before temporary chmod, and converted output-sync worker panics into deterministic batch errors.
- Replaced output-parent pathname chmod bootstrap with verified retained-inode descriptors on Linux/Android and fail-closed handling where no safe permission-independent descriptor is available.

### Fixed
- Made scan cancellation own and stop in-flight work, and prevented scan failures from exposing partial entries.
- Detect Adler-32 collisions during state migration as conflicts instead of silently treating divergent files as equivalent.
- Kept staged files and new directories private and applied final file metadata before publication.
- Made the most-specific location rule win, with later rules winning for equivalent paths, and fixed `+.` and bare `+` root includes.
- Report ignored and excluded children separately when they block destination directory removal, instead of presenting all blockers as unexpected children.
- Kept ignored directory removal blockers blocking unless `--prune-ignored` is explicitly supplied or the pattern is listed in `[prune]`.
- Ignored symlink permission bits when rechecking remove/replace targets, matching Duet's existing symlink metadata behavior.

## 0.8.8 - 2026-06-20

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
