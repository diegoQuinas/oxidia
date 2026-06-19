# Proposal: Creatures drop corpse with loot

## Intent

Monsters and players die with nothing — no corpse sprite, no loot on the ground. This breaks the core Tibia looting loop. We need creatures to drop a visible corpse and spawn loot items with chance-based rolls so players have a reason to kill and collect.

## Scope

### In Scope
- Parse `corpse` id from `<look>` in monster XML definitions
- Parse `<loot>` blocks from monster XMLs into `MonsterDrop` entries
- Spawn corpse item on death tile for monsters (decorative, non-container)
- Drop loot items on ground around corpse based on chance rolls
- Player death reuses same corpse+loot system (equipment/backpack as loot pool)
- Corpse is decorative — no container interaction in MVP

### Out of Scope
- Corpse decay (deferred — corpses persist until restart)
- Corpse as container (double-click to loot)
- Nested loot (items inside containers inside corpse)
- Player loot pool customization (which items drop on death)

## Capabilities

### New Capabilities
- `creature-loot`: Loot table structs, XML parsing from `<loot>` blocks, chance-based rolling, item ID resolution
- `creature-death`: Death flow for monsters AND players — spawn corpse item on death tile, drop loot items via chance rolls, unify the death-loot pipeline

### Modified Capabilities
None — no existing specs cover death or loot behavior.

## Approach

1. Add `look_corpse: u16` to `MonsterType`, parse from `<look corpse="...">` in both XML parsers
2. Parse `<loot>` sections from individual monster XMLs — each `<item id="..." chance="..."/>` becomes a `MonsterDrop`
3. Spawn corpse in `do_monster_death()` via overlay system (same pattern as `spawn_splash`), positioned at death tile
4. Keep `spawn_loot()` for loose ground items — it already rolls `MonsterDrop.chance`
5. Player death (`do_death`): collect player inventory + open containers as a loot pool, spawn corpse + drop items on death tile before save-to-temple
6. Default `look_corpse = 0` means no corpse (safe for existing tests)

## Affected Areas

| Area | Impact | Description |
|------|--------|-------------|
| `world/src/game/monster.rs` | Modified | Add `look_corpse: u16` to `MonsterType`, parse corpse + loot from XML |
| `world/src/game/combat.rs` | Modified | `do_monster_death` spawns corpse; `do_death` spawns corpse+player loot |
| `world/src/game/mod.rs` | Modified | Thread `look_corpse` through spawn/respawn loaders |

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Tile cap (10 things) overflow | Med | Check cap before spawning, same as `spawn_loot` already does |
| Corpse item ID not in items.otb | Low | All corpse IDs in existing XMLs are valid (verified in exploration) |
| Existing tests break from loot parse | Low | `look_corpse: 0` = skip corpse; existing monsters have no `<loot>` blocks |

## Rollback Plan

Revert the changes to `monster.rs`, `combat.rs`, and `mod.rs`. Loot parsing and corpse spawning are purely additive — removing them restores the original no-corpse behavior without data loss.

## Dependencies

None.

## Success Criteria

- [ ] Monster death spawns corpse sprite on death tile (visible to spectators)
- [ ] Loot items appear on ground around corpse with correct probability
- [ ] Player death spawns corpse + drops inventory items on death tile
- [ ] All existing tests pass unchanged (`cargo test`)
