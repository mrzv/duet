# Comparison: PERMISSIONS.md vs PERMISSIONS-DS4F.md

## Overview

| Aspect | PERMISSIONS.md | PERMISSIONS-DS4F.md |
|--------|---------------|---------------------|
| **Length** | 312 lines | 90 lines |
| **Style** | Narrative prose | Tabular, structured |
| **Issues tracked** | 12 broad categories | 28 specific items |
| **Code references** | General area (e.g. `src/sync.rs`) | Exact `file:line` |
| **Severity classification** | No | Critical / High / Medium / Design |
| **Fix recommendations** | Yes, with priority order | Via severity table |
| **Execution phase mapping** | No | Yes (Profile Parsing → State Saving) |

## Overlap

Both documents identify the same core problems:

- Scan failures swallowed as deletions (most dangerous)
- State file loading/saving with `unwrap`/`expect`
- Apply-phase filesystem operations panicking
- Concurrent apply with no rollback
- Missing uid/gid, symlink `read_link` silently swallowing errors
- Remote error detail loss
- Scanner `expect`/`unwrap` for metadata and directory reads

Both agree on the root cause: **every filesystem operation uses `.expect()` or `.unwrap()`**, treating any I/O error as a fatal crash rather than a recoverable condition.

## Unique to PERMISSIONS.md

- Directory modes blocking future syncs (#8)
- Mode-bit sync model documentation (#9)
- Restricted sync ancestor traversal (#11)
- Server setup/support file permission paths (#12)
- **A recommended fix order** (most valuable differentiator)

## Unique to PERMISSIONS-DS4F.md

- Profile parsing panics (e.g. `$HOME` unset)
- `set_base()` home directory expansion failure
- SSH key permission errors (`chmod 600` hint missing)
- Per-file error recovery concept (#25)
- Explicit `TODO:1-6` reference about permission-denied tracking (#26)
- Severity classification table with issue number mapping

## Summary

PERMISSIONS.md is a **work plan** — it groups problems thematically and prescribes a fix order.  
PERMISSIONS-DS4F.md is an **audit** — it exhaustively inventories every `expect`/`unwrap` site with exact locations and severity.

The two documents are complementary; PERMISSIONS-DS4F.md is more thorough for verification, while PERMISSIONS.md is more actionable for implementation planning.
