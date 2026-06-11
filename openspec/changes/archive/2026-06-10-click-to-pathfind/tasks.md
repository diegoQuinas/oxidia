# Tasks: Click-to-Move Pathfinding

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~510 |
| 400-line budget risk | Medium |
| Chained PRs recommended | No |
| Suggested split | Single PR |
| Delivery strategy | ask-on-risk |
| Chain strategy | pending |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: pending
400-line budget risk: Medium

### Suggested Work Units

| Unit | Goal | Likely PR | Notes |
|------|------|-----------|-------|
| 1 | Full click-to-move | PR 1 | Single PR; all tasks included with tests |

## Phase 1: Protocol Layer

- [x] 1.1 `walk.rs`: Add `auto_walk_destination(start, &[AutoWalkStep]) -> Option<Position>` — derive target by applying step deltas; test known steps + overflow
- [x] 1.2 `game_service.rs`: Extend `0xBE` handler to call `world.clear_auto_walk(id)` alongside `set_target(0)`

## Phase 2: World State & Commands

- [x] 2.1 `mod.rs`: Add `go_to_position: Option<Position>` to `PlayerState` (after `follow_target`)
- [x] 2.2 `test_support.rs`: Set `go_to_position: None` in `add_player()`
- [x] 2.3 `mod.rs`: Add `Command::GoToPosition { id: u32, target: Position }` and `Command::ClearAutoWalk { id: u32 }` variants to `Command` enum
- [x] 2.4 `mod.rs`: Add `WorldHandle::goto_position()` and `WorldHandle::clear_auto_walk()` methods; wire dispatch in `Game::handle()`

## Phase 3: Core Logic

- [x] 3.1 `mod.rs`: Implement `do_go_to_position()` — validate (same-floor, walkable, viewport, not-PZ, not-already-there), collect creatures, compute A* via `get_path_matching()`, fill `list_walk_dir`, cancel `follow_target`; status message on rejection
- [x] 3.2 `mod.rs`: In `Game::handle()`, `Command::Move` clears `follow_target`, `go_to_position`, and `list_walk_dir` before calling `do_move()`
- [x] 3.3 `mod.rs`: Extend `on_monster_ai_tick()` player loop — filter players by `go_to_position.is_some()`, apply PZ/arrival/queue-empty/blocked-path cancellation, pop dir, `do_move(id, dir)`; 3-consecutive-repath-failure termination

## Phase 4: Server Wiring

- [x] 4.1 `game_service.rs`: Wire `0x64` handler — parse steps via `walk::parse_auto_walk()`, derive target via `auto_walk_destination()`, call `world.goto_position()`; remove old debug-only stub

## Phase 5: Tests

- [x] 5.1 Walkable destination → player walks to target
- [x] 5.2 Unwalkable dest → status message, no path
- [x] 5.3 Cross-floor dest → rejected
- [x] 5.4 PZ dest → rejected; PZ mid-walk → clear goto
- [x] 5.5 Manual step cancels goto + queue
- [x] 5.6 ESC cancels goto + queue + attack
- [x] 5.7 No path → status message + clear goto
- [x] 5.8 3+ repath failures → terminate auto-walk
- [x] 5.9 Arrival → "You have arrived." + clear goto
- [x] 5.10 Out-of-viewport click → rejected silently

## Phase 6: Cleanup

- [x] 6.1 Remove old `"auto-walk GoTo not implemented"` debug stub in `game_service.rs`
- [x] 6.2 `cargo clippy -D` pass — fix any new warnings
