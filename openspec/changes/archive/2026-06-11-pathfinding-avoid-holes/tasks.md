# Tasks: Pathfinding should avoid holes

## Review Workload Forecast

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: pending
400-line budget risk: Low

Estimated changed lines: 150-300. Production changes are 2 lines; bulk is new tests.

## Phase 1: RED — Write failing tests

- [x] 1.1 `crates/world/src/map.rs` tests — `get_path_matching` rejects `FloorChange::DOWN` tile as intermediate step (hole between start and target, assert path avoids or returns empty)
- [x] 1.2 `crates/world/src/game/movement.rs` tests — `do_move_monster` rejects `FloorChange::DOWN` destination (monster adjacent to hole, step toward it returns early)
- [x] 1.3 `crates/world/src/game/mod.rs` tests — `do_go_to_position` routes around hole (hole between player and target, path must go around, not through)
- [x] 1.4 `crates/world/src/game/mod.rs` tests — `do_follow_target` routes around hole (follow target across map with intermediate hole)

## Phase 2: GREEN — Implement production fix

- [x] 2.1 `crates/world/src/map.rs:587` — add `&& self.floor_change_at(x as i32, y as i32, z as i32).is_empty()` to `get_path_matching` walkability closure
- [x] 2.2 `crates/world/src/game/movement.rs:463` — add `&& self.chunks.floor_change_at(d.x as i32, d.y as i32, d.z as i32).is_empty()` to `do_move_monster` step filter
- [x] 2.3 `cargo test` — verify new tests pass AND existing tests pass (manual step onto hole still works)

## Phase 3: Verify

- [x] 3.1 `cargo clippy --all-targets -- -D warnings`
- [x] 3.2 `cargo test` — full 400+ test suite
- [x] 3.3 Confirm existing `stair_map` tests (`walking_onto_a_down_stair_drops_a_floor` et al.) still pass — manual floor-change is unaffected
