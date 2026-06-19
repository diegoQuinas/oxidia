## Exploration: Creatures drop corpse with loot on death

### Current State

**Monster death** is implemented in `crates/world/src/game/combat.rs`:
- `do_monster_death()` (line 421): when HP hits 0, creates death splash, clears fights, removes the monster, schedules respawn, calls `spawn_loot()`
- `spawn_loot()` (line 513): rolls loot table entries and drops items directly on the death tile as loose ground items via the copy-on-write overlay system
- `apply_monster_damage()` (line 310): applies damage, pushes health-bar/effects/splash, calls `do_monster_death()` on 0 HP

**Loot system (existing):**
- `MonsterDrop` struct (monster.rs:342-350): `item_id`, `chance: f64`, `count: u8`
- `MonsterType.loot`: `Vec<MonsterDrop>` — always empty in both `parse_monsters_xml()` and `parse_monsters_data_dir()` — **no loot is parsed from XML**
- Loot drops directly on the ground as individual items (not inside a corpse container)
- The existing test monsters get `loot: vec![]`, so `spawn_loot()` is effectively a no-op today

**No corpse exists:**
- Individual monster XMLs have `<look type="21" corpse="5964"/>` — the `corpse` attribute is **not parsed**
- `MonsterType` has no `corpse_id` / `look_corpse` field
- No corpse item is created on death
- Loot items spawn as loose items on the ground, but since no loot is parsed, this code path produces nothing visible

**Containers system exists (M10.3):**
- `containers.rs` has full container functionality: open/close/nested containers, move items in/out, `ContainerSource::Ground` for ground containers
- Ground containers opened via `0x82` create an `OpenContainer` with **empty items** (`Vec::new()`) — there is NO mechanism for pre-populated ground container contents
- `ItemMeta.is_container` and `container_capacity` are resolved from items.xml

**Corpse items in data:**
- `items.xml` has many corpse definitions (e.g., `id="5964"` dead rat at line 9611) with:
  - `containerSize` (e.g., 10) — makes them containers
  - `decayTo` (e.g., "3073") — decay chain
  - `duration` (e.g., 10 seconds) — how long before decay
  - `corpseType` (e.g., "blood") — matches the monster's race for splash
  - `fluidSource` (e.g., "blood") — for the death splash
- These items exist in items.otb and are resolvable via `meta.item_meta()`

**Splash system (already implemented in M7.1/M7.2):**
- `RaceType` enum with `Blood`, `Venom`, etc.
- `MonsterState.race: Option<RaceType>` — populated from XML
- `spawn_splash()` creates full splash on death and small splash on hit

**XML parsing gaps:**
- `parse_monsters_data_dir()` parses `race`, `look type`, `health`, `speed`, `attack` from individual XMLs
- Does NOT parse `corpse` from `<look>` element
- Does NOT parse `<loot>` sections
- `parse_monsters_xml()` (flat config) also doesn't parse `corpse` or `loot`

### Affected Areas

- `crates/world/src/game/monster.rs` — `MonsterType` needs `look_corpse: u16` field; `parse_monsters_data_dir()` needs to parse `corpse` from `<look>` and `<loot>` sections; `MonsterState`/`MonsterSpawn` need to carry the corpse id
- `crates/world/src/game/combat.rs` — `do_monster_death()` needs to spawn a corpse item before/instead of loose loot; `spawn_loot()` needs rework to place loot in corpse container or keep as-is
- `crates/world/src/game/mod.rs` — `load_spawns()` and `process_respawns()` need to pass `look_corpse` to `MonsterState`/`MonsterSpawn`
- `crates/world/src/game/containers.rs` — if going the corpse-as-container route, needs mechanism for pre-populated ground container contents
- `crates/world/src/map.rs` — `TileStack` might need extension for per-item container contents
- `data/monster/` — monster XMLs already have `corpse` attribute (no change needed)
- `data/items/items.xml` — corpse items already defined (no change needed)

### Approaches

1. **Minimal: Corpse as decorative tile item + loose loot** — Add `corpse_id` to `MonsterType`, parse from XML, spawn the corpse item on death as a non-interactive tile item. Loot stays as loose ground items. No container/double-click-to-loot. No decay (corpses persist until server restart).
   - Pros: Minimal changes, works immediately, loot is visible and pickable
   - Cons: Not TFS-faithful (corpses are non-functional), no decay, no "loot inside corpse" mechanic
   - Effort: Low (2-3 files, ~50 lines)

2. **Corpse as container, loot inside + decay** — Parse corpse id, create the corpse as a container on the ground, place loot items inside the corpse container, implement decay system so corpses degrade and disappear. Requires pre-populated ground container contents mechanism.
   - Pros: TFS-faithful, proper looting UX, containers system reused
   - Cons: Requires extending TileStack or adding a ground-container-contents table, decay system needs a new tick or hook into existing ticks, higher complexity
   - Effort: High (6-8 files, ~300+ lines)

3. **Hybrid: Corpse as container (no loot inside) + loose loot** — Corpse spawns as a real container (players can open it to see empty), loot items are still on the ground separately. This gives the visual of a corpse while keeping loot accessible.
   - Pros: Loot is immediately visible and pickable, corpse is real container
   - Cons: Confusing UX (loot on ground AND empty corpse), still need pre-populated container to avoid empty corpse
   - Effort: Medium

4. **Corpse item + loot inside (no decay)** — Full TFS behavior minus decay. Spawn corpse as container, pre-populate with loot. Players must open the corpse to get the loot. Loot is NOT on the ground separately.
   - Pros: TFS-faithful looting experience
   - Cons: Requires ground container contents mechanism, corpse never decays (stays forever), loot not visible until opened
   - Effort: Medium-High

### Recommendation

**Approach 1 (Minimal)** is right for M13. Here's why:

1. **No loot parsing exists yet** — `MonsterType.loot` is always `vec![]` in both XML parsers. The first step is parsing loot tables AND corpse ids from monster XMLs, then actually dropping loot. Without corpse items, the loot becomes visible and pickable — that's already a huge improvement from the current (non-functional) state.

2. **Decay is deferred** — already identified in the splash exploration as a stretch goal. Decay needs a `DecayTick` command, a `decay_track: Vec<...>` registry, and `decayTo`/`duration` resolution from items.xml. That's a separate change.

3. **Loot-in-container needs architectural work** — ground container contents aren't pre-populated anywhere today. The container system expects contents to come from player inventory persistence, not from death rolls. This is a non-trivial extension.

4. **Immediate user-facing value** — the user wants creatures to drop a corpse with loot on death. With Approach 1, the corpse sprite appears on the ground (visual confirmation), and loot items appear next to it (pickable). This demonstrates the feature working end-to-end.

**Recommended next steps:**
1. Parse `corpse` from `<look>` in individual monster XMLs and `MonsterSpawn` from spawn XML defaults
2. Add `look_corpse: u16` to `MonsterType`/`MonsterState`/`MonsterSpawn`
3. Spawn the corpse item on death via the overlay system (mirroring `spawn_splash` pattern)
4. Actually parse `<loot>` sections from individual monster XMLs into `MonsterDrop` structs
5. Keep existing `spawn_loot()` behavior for loose items
6. When decay system is ready: wrap corpses + splashes in decay, and optionally containerize corpse loot

### Risks

- **No decay** means corpses accumulate on death tiles and persist until server restart — acceptable for M13
- **Loot parsing from XML** is critical and doesn't exist yet — must handle all edge cases (chance/maximum, nested containers, action ids, text descriptions)
- **Tile cap (10 things)** — the corpse + splash + multiple loot items can hit the cap quickly. Must check before spawning (as `spawn_loot` already does)
- **Corpse item must exist in items.otb** — all corpse ids in monster XMLs are valid item ids in items.otb (verified: 5964=dead rat, 2817=dead snake, etc.)
- **Corpse items are containers in items.xml** — the OTB group will be `ITEM_GROUP_CONTAINER`; the wire form carries no subtype for containers (unlike splashes). The `broadcast_dest` path handles this correctly via `WireItem.subtype = None` for containers.
- **Existing tests add monsters with no corpse id** — the field should default to 0 (no corpse), and `spawn_corpse` should skip when id == 0

### Ready for Proposal

Yes. The exploration is complete. This maps to **M13** (loot & corpses). The implementation splits into two clear slices:

1. **M13.1** — Parse `corpse` + `<loot>` from monster XMLs, spawn corpse item + loot on death (this exploration's recommended approach)
2. **M13.2** — Add decay system for corpses, splashes, and other timed items (stretch goal)

The orchestrator should tell the user the feature ships in two slices, with M13.1 providing immediate visible corpses + lootable items on the ground.
