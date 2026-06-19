# Design: Creatures drop corpse with loot

## Technical Approach

Extend the existing creature death flow (`do_monster_death`, `do_death`) with corpse spawning and loot pools. Corpse follows the same overlay-insert pattern as `spawn_splash` (materialize → cap check → `pre_creature_len` insert → `broadcast_dest`). Monster loot parsing from `<loot>` XML blocks feeds into the existing `MonsterDrop`/`spawn_loot` pipeline. Player death collects inventory + backpack contents as guaranteed loot. No new abstractions — the game actor's existing overlay system handles everything.

## Architecture Decisions

| Option | Tradeoff | Decision |
|--------|----------|----------|
| Container-based corpse vs decorative corpse | Container opens the looting UX but is complex to wire up with existing overlay | **Decorative** — corpse is a static item on the tile, no interaction |
| Separate `roll_drops()` vs roll inside `spawn_loot` | `roll_drops` is cleaner separation, but `spawn_loot` already rolls chance internally | **Add `roll_drops(&[MonsterDrop]) → Vec<(u16, u8)>`** as spec requires; refactor `spawn_loot` to accept `&[(u16, u8)]` for placement-only |
| `look_corpse` on `MonsterState` vs lookup from type at death | Monsters are cloned from types at spawn; putting it on `MonsterState` is simpler and avoids a HashMap lookup in the hot death path | **On `MonsterState`** (and `MonsterType` / `MonsterSpawn` for consistent cloning) |
| `count` vs `count_max` on `MonsterDrop` | Existing field is `count: u8`, spec calls for `count_max: u16`. Rename + widen for spec compliance | **Rename to `count_max: u16`**, add random count `1..=count_max` in `roll_drops` |

## Data Flow

```
Monster death:
  do_monster_death(m)
    → spawn_splash(death_pos, FULLSPLASH, race_fluid)
    → spawn_corpse(death_pos, m.look_corpse) ─── overlay: insert at pre_creature_len, broadcast 0x6A
    → remove creature from tile (0x6C broadcast)
    → monsters.remove(id)
    → let rolled = roll_drops(&m.loot) ───────── independent chance + random count 1..=count_max
    → spawn_loot(death_pos, &rolled) ─────────── overlay: insert at pre_creature_len, broadcast 0x6A, cap check

Player death:
  do_death(player_id)
    → spawn_splash(death_pos, FULLSPLASH, race_fluid)
    → spawn_corpse(death_pos, ITEM_HUMAN_CORPSE=3058) ─── same overlay pattern
    → collect_player_loot(inventory, containers) ──────────→ Vec<(u16, u8)>
    → spawn_loot(death_pos, &loot) ───────────────────────── same placement path
    → remove player from tile, save to temple (unchanged)
```

## File Changes

| File | Action | Description |
|------|--------|-------------|
| `crates/world/src/game/monster.rs` | Modify | Add `look_corpse: u16` to `MonsterType`, `MonsterSpawn`, `MonsterState`; parse `<look corpse="X"/>` + `<loot>` in `parse_monsters_data_dir`; rename `MonsterDrop.count` → `count_max: u16`; add `roll_drops()` |
| `crates/world/src/game/combat.rs` | Modify | Add `spawn_corpse()` helper; call it from `do_monster_death` and `do_death`; add `collect_player_loot()`; refactor `spawn_loot` to accept `&[(u16, u8)]` after pre-rolling |
| `crates/world/src/game/mod.rs` | Modify | Add `ITEM_HUMAN_CORPSE: u16 = 3058`; thread `look_corpse` through `load_spawns` |
| `crates/world/src/game/test_support.rs` | Modify | Add `look_corpse: 0` to `add_monster` and `add_player` test constructors |
| `crates/world/src/game/containers.rs` | No change | Existing `export_container_items` pattern reused for loot collection |

## Interfaces / Contracts

**New/Modified types** (all in `monster.rs`):
```rust
pub(crate) struct MonsterDrop {
    pub(crate) item_id: u16,
    pub(crate) chance: f64,          // 0.0..=1.0
    pub(crate) count_max: u16,       // was `count: u8`
}

/// Pre-roll drops independently by chance, randomize count 1..=count_max.
pub(crate) fn roll_drops(rng: &mut StdRng, drops: &[MonsterDrop]) -> Vec<(u16, u8)>;
```

**New helpers** (in `combat.rs`):
```rust
/// Spawn a corpse item at pos via overlay (same pattern as spawn_splash).
/// Silently skips if look_corpse == 0 or tile at capacity.
fn spawn_corpse(&mut self, pos: Position, corpse_id: u16);

/// Collect player inventory + backpack contents as guaranteed loot drops.
fn collect_player_loot(
    inventory: &[Option<InvItem>; 10],
    containers: &[Option<OpenContainer>; 16],
) -> Vec<(u16, u8)>;
```

**Changed signature** (existing contract):
```rust
// Was: spawn_loot(&mut self, pos: Position, loot: &[MonsterDrop])
// Now: receives already-rolled drops, just places them
fn spawn_loot(&mut self, pos: Position, loot: &[(u16, u8)]);
```

## Testing Strategy

| Layer | What to Test | Approach |
|-------|-------------|----------|
| Unit | `roll_drops` correctness | Seeded RNG: verify independent chance, count range `1..=count_max`, all-succeed/all-fail |
| Integration | XML parse → `MonsterType.loot` + `look_corpse` | Parse sample XML with loot blocks, verify struct fields |
| Integration | Monster death spawns corpse | `do_monster_death` → check `dynamic` tile contains corpse server_id |
| Integration | Monster loot on ground | Same pattern as existing `monster_drops_item_on_death` test (0x6A packet assertion) |
| Integration | Player death spawns corpse + loot | `do_death` → check tile for 3058 corpse, check ground for inventory items |
| Integration | Tile cap respected | Fill tile with 10 items, kill monster, verify no panic |
| Integration | Player empty inventory | Kill player with no items → only corpse spawns, no loot |
| E2E | Full client-visible flow | `broadcast_dest` assertion on 0x6A packets for both death types |

## Migration / Rollout

No migration required. Existing save data is unaffected. `look_corpse: 0` + empty loot in existing type definitions means zero behavioral change for existing tests and deployed monsters until XML is updated.

## Open Questions

- [ ] Player backpack detection: which inventory slot(s) are "backpack" for container-content collection? Use any open container with `Slot(u8)` source.
- [ ] Wire position for corpse vs loot: corpse at `pre_creature_len`, loot stacked after it — does the client render them correctly? Need to verify stackpos assignment.
