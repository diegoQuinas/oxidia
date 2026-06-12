## Exploration: Blood item on hit (TFS-style)

### Current State

The server has a complete combat system (M7) that applies damage and pushes the **EFFECT_DRAWBLOOD** magic-effect animation (a visual splash above the creature), but **no floor item is created** — there are no blood pools/splashes on the ground. The existing code path in `apply_damage()` / `apply_monster_damage()` (`crates/world/src/game/combat.rs`):
- Reduces HP and pushes `0x8C` health-bar + `0xA0` stats
- Pushes `EFFECT_DRAWBLOOD` magic effect (animation only)
- Pushes floating damage text via `0xB4`
- On death, removes the creature and spawns loot via `spawn_loot()`

The **copy-on-write overlay system** (`materialize`, `dynamic`, `add_to_ground_front`, `broadcast_dest`) already supports spawning items on tiles — proven by `spawn_loot()` in `do_monster_death()`. The **item system** already handles splash/fluid items via `is_fluid_or_splash()` in `formats::otb`, and the wire encoder already writes a subtype byte for fluid-type items.

**Key gap:** No `race` (health type) field exists on `MonsterType` / `MonsterState` or `PlayerState`. No decay system exists for items.

### TFS Reference Behavior

**On combat hit** (`reference/tfs/src/game.cpp:3874-3915` — `Game::combatGetTypeInfo`):
- Creates `ITEM_SMALLSPLASH` (2019) on target's tile, subtype = fluid type
- `RACE_VENOM` → `FLUID_SLIME` (green, wire = 3)
- `RACE_BLOOD` → `FLUID_BLOOD` (red, wire = 1)
- `RACE_UNDEAD` / `RACE_FIRE` / `RACE_ENERGY` → no splash
- Starts decay on the splash item

**On creature death** (`reference/tfs/src/creature.cpp:727-746` — `Creature::dropCorpse`):
- Creates `ITEM_FULLSPLASH` (2016) on death tile, same fluid-type logic
- Starts decay

**Item data** (`data/items/items.xml` lines 1684-1707):
- `2016` (full splash): decays `2016→2017→2018→0` (45s/45s/600s)
- `2019` (small splash): decays `2019→2020→2021→0` (45s/45s/60s)
- Group = `ITEM_GROUP_SPLASH` (11) → `is_fluid_or_splash()` = true

**Fluid types** (`reference/tfs/src/const.h`):
- `FLUID_BLOOD = 1` (FLUID_RED), `FLUID_SLIME = 3` (FLUID_GREEN)
- Wire subtype byte renders the appropriate colored pool on the client

### Affected Areas

- `crates/world/src/game/mod.rs` — `PlayerState` needs `race` field, `MonsterState` already exists, `Game` needs `RaceType` enum + `spawn_splash()` method
- `crates/world/src/game/monster.rs` — `MonsterType` / `MonsterState` need `race` field; `parse_monsters_xml` needs `race` attribute parsing
- `crates/world/src/game/combat.rs` — `apply_damage()` and `apply_monster_damage()` need to spawn splash on hit; `do_monster_death()` and `do_death()` need to spawn full splash on death
- `crates/world/src/game/items.rs` — broadcast helpers already exist (`broadcast_dest`), may be reused
- `crates/world/src/game/condition.rs` or `mod.rs` — optional decay system for splash items
- `crates/world/src/map.rs` — `wire_item()` already handles splash/fluid subtype in line 141
- `data/items/items.xml` — already has decay chains for splashes (no changes needed)
- `config/monsters.xml` — needs `race` attribute on monster entries

### Approaches

1. **Minimal: race field + splash spawn (no decay)** — Add `RaceType` enum to world crate, add `race` field to `MonsterType`/`MonsterState`/`PlayerState`, spawn `ITEM_SMALLSPLASH` on hit and `ITEM_FULLSPLASH` on death. Skip item decay — splash stays forever (or until server restart, since the overlay is not persisted).
   - Pros: Minimal changes, works immediately, matches TFS mechanics
   - Cons: Splashes never decay (not TFS-faithful), no decay system
   - Effort: Low

2. **Full: race field + splash spawn + decay tick** — Same as approach 1 but also implement an item decay system: a per-item `decay_to` / `duration` reference from `items.xml`, and a `DecayTick` command on the actor that iterates decay-tracked items and transforms or removes them.
   - Pros: TFS-faithful, splash pools dry up
   - Cons: More complex, requires decay data model and tick scheduling
   - Effort: High

3. **Hit splash only (no death splash)** — Only spawn `ITEM_SMALLSPLASH` on combat hits, skip the full splash on death. Useful if death is handled separately (M13 corpses).
   - Pros: Simplest possible, covers the main visible effect
   - Cons: Missing a key TFS behavior, bare ground on death looks odd
   - Effort: Very Low

### Recommendation

**Approach 1** (race field + both splashes, no decay) — it's the sweet spot. The race/fluid-type mechanic is central to TFS blood behavior, the overlay system already supports item spawning, and splash decay is a stretch goal. The PROGRESS.md already deferred decay as a later concern ("floor blood splat (ITEM_SMALLSPLASH + decay) → M9" was deferred). Splashes reset on server restart (the overlay is in-memory only), which matches TFS's non-persistent behavior — decay just makes them look nicer mid-session.

### Risks

- **No decay means splashes accumulate on popular PvP tiles** — acceptable for now; TFS also has them persist for at least 45-60s per stage
- **Race attribute must be added to monsters.xml** — existing spawn entries lack it, must default to `RACE_BLOOD` for backward compat
- **Player race is always `RACE_BLOOD`** — matches TFS `player.h:511`
- **Splash items (2016/2019) must exist in items.otb + items.xml** — verified they do in the real files
- **Item count/subtype for fluid items** — the subtype byte carries the fluid type (1=blood, 3=slime), not a count. The existing `wire_item()` function already handles this: `is_fluid_or_splash()` items pass the subtype through
- **Tile cap (10 things)** — splashes count toward the 10-thing cap. Must check before spawning (same as `spawn_loot()` does)

### Ready for Proposal

Yes — the exploration is complete. The orchestrator should tell the user this maps to **M13** (loot & corpses) with overlap into M7.2 (hit effects). The implementation adds a `RaceType` enum, race fields to creatures, splash spawn methods, and leverages the existing copy-on-write overlay + broadcast system. No model changes to the item system needed.
