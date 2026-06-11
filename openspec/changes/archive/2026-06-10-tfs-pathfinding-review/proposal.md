# Proposal: TFS Pathfinding Review

## Intent

Fix three bugs in the TFS pathfinding system causing wrong destinations, unnecessary diagonals, and unresponsiveness on rapid clicks. The root cause is a `last_pos` cache drift in `game_service.rs` that desyncs GoTo target derivation on blocked moves.

## Scope

### In Scope

1. **Fix `last_pos` cache drift** — Stop deriving GoTo targets from a position cache that desyncs on blocked moves. Use actor-authoritative position or confirmation-based cache updates.
2. **Skip redundant A\* on identical target** — In `do_go_to_position`, skip pathfinding if target equals current `go_to_position` AND `list_walk_dir` is non-empty.
3. **Align neighbor pruning with TFS** — Replace pruning tables in `pathfinding.rs` to match TFS `dirNeighbors` exactly.

### Out of Scope

- `last_walk_ms` timer-before-do-move fix (LOW, deferred)
- Full walk-event rescheduling (requires TFS event model refactor)
- `last_pos` cache removal (deferred to maintenance)

## Capabilities

### New Capabilities

None — pure internal bugfixes, no new system capabilities.

### Modified Capabilities

None — no spec-level behavior changes. Existing `player-auto-walk` REQs remain unchanged (fixes affect correctness of existing specs, not their requirements).

## Approach

Three independent fixes applied sequentially:

1. **`game_service.rs`**: Replace optimistic `last_pos` update with a confirmation-based model — only update on `cancel_walk` feedback, or use the actor's authoritative position for GoTo target derivation (recommended: derive target from client path instead of cached position + direction offsets).
2. **`game/mod.rs`**: In `do_go_to_position`, compare new target against `go_to_position`; if same and walk queue is non-empty, skip A\* entirely.
3. **`pathfinding.rs`**: Replace `neighbors_with_pruning()` lookup table with TFS `dirNeighbors` constants from `reference/tfs/src/map.cpp`.

Each fix has its own test verifying correctness before implementation (strict TDD).

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `crates/server/src/game_service.rs` | Modified | `last_pos` cache update logic in 0x64 handler |
| `crates/world/src/game/mod.rs` | Modified | Redundant A\* guard in `do_go_to_position` |
| `crates/world/src/pathfinding.rs` | Modified | Neighbor pruning table in `neighbors_with_pruning` |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Pruning change alters existing path tests | Med | Update test expectations to match TFS output |
| Confirmation-based cache adds latency | Low | Oneshot channel is negligible overhead |

## Rollback Plan

`git revert` each commit individually (three atomic commits). Run `cargo test` after each revert to confirm no regression.

## Dependencies

- TFS reference source (`reference/tfs/src/map.cpp`) for authoritative `dirNeighbors` table.

## Success Criteria

- [ ] `last_pos` no longer drifts after a blocked move (verified by new test)
- [ ] Repeated click on same target skips A\* pathfinding (verified by new test + assertion)
- [ ] Neighbor pruning matches TFS `dirNeighbors` exactly (verified by table-audit test)
- [ ] `cargo test` passes with no regressions
