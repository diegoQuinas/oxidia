# Tasks: GM Speed Command

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~200 (180–220) |
| 400-line budget risk | Low |
| 800-line budget risk | Low |
| Chained PRs recommended | No |
| Suggested split | Single PR |
| Delivery strategy | auto-forecast |
| Chain strategy | pending |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: pending
400-line budget risk: Low
800-line budget risk: Low

## Phase 1: Player Speed Field

- [x] 1.1 Add `speed: u16` to `PlayerState` struct in `crates/world/src/game/mod.rs`
- [x] 1.2 Update `creature_speed()` in `mod.rs` to read `p.speed` for players
- [x] 1.3 Update `on_regen_tick` stats push in `mod.rs` to use `p.speed`
- [x] 1.4 Update `combat.rs` stats push to use `p.speed` instead of `220`
- [x] 1.5 Add `speed: 220` to `add_player()` in `test_support.rs`
- [x] 1.6 Add `speed: 220` to all 7 `PlayerState` constructions in `session.rs`
- [x] 1.7 Add `speed: 220` to test `PlayerState` in `look.rs`

## Phase 2: GM Command Core

- [x] 2.1 Add `Speed` variant to `GmVerb` enum, `ALL` list, `words/usage/description`
- [x] 2.2 Add dispatch arm in `do_gm_command` match
- [x] 2.3 Implement `gm_speed()`: parse optional name + u16, validate 10..=2500, set `p.speed`, push 0xA0 stats, remove+re-introduce for spectators, push status message

## Phase 3: Verification

- [x] 3.1 Extend `gmverb_registry_is_complete_and_resolvable` for Speed variant
- [x] 3.2 Test: `/speed 500` self-target sets speed, pushes 0xA0
- [x] 3.3 Test: named-target sets target speed + invalid name error
- [x] 3.4 Test: range validation (9 error, 2501 error, 10 ok, 2500 ok, non-numeric error)
- [x] 3.5 Test: spectator receives remove+re-introduce after speed change
- [x] 3.6 Test: `creature_speed()` reads from `PlayerState.speed`
- [x] 3.7 Test: stats push uses live `p.speed` (regen or combat)
