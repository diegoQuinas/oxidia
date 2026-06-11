# Tasks: TFS Pathfinding Review

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | 120–180 |
| 800-line budget risk | Low |
| Chained PRs recommended | No |
| Suggested split | Single PR |
| Delivery strategy | single-pr |
| Chain strategy | size-exception |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: size-exception
400-line budget risk: Low

Three independent fixes, same subsystem, no cross-cutting concerns. Single PR well within 800-line review budget. TDD active — each fix gets RED (failing test) then GREEN (implementation).

## Phase 1: Foundation

- [x] 1.1 Add `Command::GoToSteps { id: u32, steps: Vec<protocol::walk::AutoWalkStep> }` variant to command enum in `crates/world/src/game/mod.rs`

## Phase 2: Fix 1 — `last_pos` cache drift (TDD)

- [x] 2.1 [RED] Test: stale `last_pos` must not corrupt GoTo target; assert derived target uses actor `p.position`, not cache
- [x] 2.2 [GREEN] Modify `crates/server/src/game_service.rs`: send raw 0x64 steps via `Command::GoToSteps`, stop reading `last_pos`
- [x] 2.3 [GREEN] Implement `do_go_to_steps` in `crates/world/src/game/mod.rs`: derive target from `p.position + steps`, validate, run A*

## Phase 3: Fix 2 — Redundant A* guard (TDD)

- [x] 3.1 [RED] Test: calling `do_go_to_steps` twice with same target skips A* on 2nd call
- [x] 3.2 [GREEN] Add guard in `do_go_to_steps`: if target == `go_to_position` && `list_walk_dir` non-empty, return early

## Phase 4: Fix 3 — TFS neighbor pruning (TDD)

- [x] 4.1 [RED] Test: assert `neighbors_with_pruning()` returns exact TFS `dirNeighbors` table offsets per direction
- [x] 4.2 [GREEN] Replace pruning tables in `crates/world/src/pathfinding.rs` with TFS `dirNeighbors` mapping from design

## Phase 5: Verification

- [x] 5.1 `cargo test` — all 403+ existing tests pass with zero regressions
- [x] 5.2 `cargo clippy --all-targets -- -D warnings` — no new diagnostics
- [x] 5.3 `cargo fmt` — formatting clean
