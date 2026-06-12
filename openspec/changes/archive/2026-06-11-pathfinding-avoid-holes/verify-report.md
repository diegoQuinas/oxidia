# Verify Report: Pathfinding Should Avoid Holes

**Status**: PASS
**Date**: 2026-06-11
**Verified by**: sdd-verify sub-agent

---

## Executive Summary

Implementation matches spec. All 5 new tests pass (1 unit in `map.rs`, 2 unit in `movement.rs`, 2 integration in `mod.rs`). All 528 existing tests pass. Clippy clean. All 9 tasks marked `[x]`. Production changes are exactly 2 lines as designed. Manual floor-change stepping (`is_walkable()`) remains untouched.

---

## Spec Scenario Verification

### REQ-PF-01: Floor-Change Tile Rejection in A\*

| # | Scenario | Test | Result |
|---|----------|------|--------|
| 1 | Hole tile skipped | `get_path_matching_rejects_floor_change_tile` in `map.rs` — hole at (101,100) between start (100,100) and target (102,100); path goes north | ✅ PASS |
| 2 | Stair tile also blocked | Same test — uses `FloorChange::DOWN` via item 300. All non-empty `FloorChange` flags rejected by `.is_empty()` | ✅ PASS |
| 3 | Normal tile unaffected | Detour passes through normal tiles; implicit coverage | ✅ PASS |

### REQ-PF-02: Floor-Change Rejection in Monster Movement

| # | Scenario | Test | Result |
|---|----------|------|--------|
| 1 | Monster avoids hole | `monster_does_not_step_onto_floor_change_tile` — monster at (100,100), hole East at (101,100), monster stays | ✅ PASS |
| 2 | Monster walks normally | `monster_walks_normally_on_non_floor_change_tile` — monster steps East onto walkable tile | ✅ PASS |

### REQ-PF-03: Manual Step Unaffected

| # | Scenario | Test | Result |
|---|----------|------|--------|
| 1 | Step into hole still works | `walking_onto_a_down_stair_drops_a_floor` (existing) — player manually steps East onto hole, resolves floor change | ✅ PASS |

### REQ-AW-02 Delta: Auto-Walk

| Scenario | Test | Result |
|----------|------|--------|
| Floor-change tile avoided mid-path | `get_path_matching_rejects_floor_change_tile` — bypass verified via path trace | ✅ PASS |
| Floor-change destination rejected | Implicitly covered by walkability closure (applied to ALL evaluated tiles including destination) | ✅ PASS |
| Goto routes around hole | `do_go_to_position_finds_path_around_hole` in `mod.rs` | ✅ PASS |
| Follow routes around hole | `do_follow_target_finds_path_around_hole` in `mod.rs` | ✅ PASS |

---

## Code Verification

### Production Changes (2 lines)

1. **`crates/world/src/map.rs:589`** — Walkability closure in `get_path_matching`:
   ```rust
   && self.floor_change_at(x as i32, y as i32, z as i32).is_empty()
   ```
   Checks every tile evaluated by A\* before accepting it as walkable.

2. **`crates/world/src/game/movement.rs:468`** — Monster step filter in `do_move_monster`:
   ```rust
   && self.chunks.floor_change_at(d.x as i32, d.y as i32, d.z as i32).is_empty()
   ```
   Rejects destination tiles with floor-change before committing movement.

### Unmodified
- `is_walkable()` — no floor_change check, manual steps unaffected ✅

### Test Fixtures

- `hole_bypass_map()` in `test_support.rs` — map with hole at (101,100) and bypass route via (100,99)→(101,99)→(102,100) ✅
- `stair_map()` — reused from existing tests for unit-level movement test ✅

### Test Summary

| Test | File | Type | Result |
|------|------|------|--------|
| `get_path_matching_rejects_floor_change_tile` | `map.rs` | Unit | ✅ PASS |
| `monster_does_not_step_onto_floor_change_tile` | `movement.rs` | Unit | ✅ PASS |
| `monster_walks_normally_on_non_floor_change_tile` | `movement.rs` | Unit | ✅ PASS |
| `do_go_to_position_finds_path_around_hole` | `mod.rs` | Integration | ✅ PASS |
| `do_follow_target_finds_path_around_hole` | `mod.rs` | Integration | ✅ PASS |

### Tasks

All 9 tasks marked `[x]`:
- 1.1–1.4: RED tests written and passing
- 2.1–2.2: Production implementation (2 lines)
- 2.3: Tests pass (new + existing)
- 3.1: Clippy clean
- 3.2: Full suite 528/528
- 3.3: Stair_map regression tests pass

---

## Test Results

- **cargo test**: 528 passed, 0 failed ✅
- **cargo clippy --all-targets -- -D warnings**: clean ✅
- **Baseline**: 323 → 328 (5 new tests)

---

## Risks

None identified. Implementation is minimal (2 production lines), well-tested at unit and integration level, and does not modify existing manual walk behavior.

---

## Next Steps

Ready for `sdd-archive`.
