# Tasks: Creatures drop corpse with loot

## Review Workload Forecast

| Field | Value |
|-------|-------|
| Estimated changed lines | ~530 |
| 400-line budget risk | Medium |
| Chained PRs recommended | No |
| Suggested split | single PR |
| Delivery strategy | auto-chain |
| Chain strategy | pending |

Decision needed before apply: No
Chained PRs recommended: No
Chain strategy: pending
400-line budget risk: Medium

## Phase 1: Struct & XML Foundation

- [x] 1.1 Rename `MonsterDrop.count` → `count_max: u16` in `monster.rs`
- [x] 1.2 Add `look_corpse: u16` to `MonsterType`, `MonsterSpawn`, `MonsterState` (default 0)
- [x] 1.3 Parse `<look corpse="X"/>` + `<loot>` in `parse_monsters_data_dir`
- [x] 1.4 Add `ITEM_HUMAN_CORPSE: u16 = 3058` constant in `mod.rs`

## Phase 2: Corpse & Loot Logic

- [x] 2.1 Add `pub(crate) fn roll_drops(rng, &[MonsterDrop]) -> Vec<(u16, u8)>` in `monster.rs`
- [x] 2.2 Add `fn spawn_corpse(pos, corpse_id)` overlay helper in `combat.rs`
- [x] 2.3 Add `fn collect_player_loot(inventory, containers)` in `combat.rs`
- [x] 2.4 Refactor `spawn_loot` to accept `&[(u16, u8)]` (placement-only, pre-rolled)
- [x] 2.5 Wire corpse + loot into `do_monster_death`: `spawn_corpse` → `roll_drops` → `spawn_loot`
- [x] 2.6 Wire corpse + player loot into `do_death`: `spawn_corpse` → `collect_player_loot` → `spawn_loot`
- [x] 2.7 Thread `look_corpse` through `load_spawns` in `mod.rs`

## Phase 3: Test Fixtures

- [x] 3.1 Add `look_corpse: 0` to `add_monster` + `add_player` in `test_support.rs`
- [x] 3.2 Create sample monster XML with loot + corpse for parsing tests

## Phase 4: Tests (TDD — RED/GREEN/REFACTOR order)

- [x] 4.1 `roll_drops`: seeded RNG, independent chance, count range, all-succeed/fail
- [x] 4.2 Monster death: corpse + loot on tile (assert `0x6A` packets + `server_ids`)
- [x] 4.3 Player death: item 3058 corpse + inventory items as loot pool on ground
- [x] 4.4 Tile cap: fill tile → kill → no panic, corpse + overflow silently skipped
- [x] 4.5 XML parse: loot table struct fields, empty `<loot>` = empty vec, missing countmax
- [x] 4.6 `cargo test` green: existing tests pass after all changes
