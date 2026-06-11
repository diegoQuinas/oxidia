# Delta for pathfinding-bugs

## Overview

This change fixes three internal bugs in the pathfinding/auto-walk system. No spec-level behavior changes — all existing `player-auto-walk` requirements (REQ-AW-01 through REQ-AW-10) remain unchanged. The fixes correct implementation drift without altering external contracts.

## ADDED Requirements

None — no new system capabilities or external behavior.

## MODIFIED Requirements

None — existing REQ blocks are unchanged at the spec level. Fixes affect correctness of existing implementations (e.g., `last_pos` cache no longer produces wrong destinations), but the requirements themselves remain identical.

## REMOVED Requirements

None.

## RENAMED Requirements

None.

## Internal Fixes Summary

| Fix | File | What It Corrects |
|-----|------|------------------|
| 1 | `crates/server/src/game_service.rs` | `last_pos` cache updates desync on blocked moves, causing wrong GoTo target derivation. Replaced with confirmation-based model: update only on canonical feedback. |
| 2 | `crates/world/src/game/mod.rs` | `do_go_to_position` recomputes A* on every call even when target and position are unchanged. Added idempotency guard: skip if target equals `go_to_position` AND `list_walk_dir` is non-empty. |
| 3 | `crates/world/src/pathfinding.rs` | `neighbors_with_pruning()` table differs from TFS `dirNeighbors`. Replaced with byte-for-byte match of `reference/tfs/src/map.cpp` constants. |
