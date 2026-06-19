# Creature Death Specification

## Purpose

Unified death flow for monsters and players: spawn visible corpse at death tile, roll loot from the creature's loot table, and place items on the ground around the corpse.

## Requirements

### REQ-DEATH-01: Corpse ID on MonsterType

`MonsterType` MUST have `look_corpse: u16` parsed from `<look corpse="X"/>`. A value of `0` means no corpse.

**Scenario: Corpse from XML** — GIVEN `<look corpse="3058"/>`, WHEN parsed, THEN `look_corpse` MUST be `3058`.

**Scenario: Default no corpse** — GIVEN no `corpse` attribute, WHEN parsed, THEN `look_corpse` MUST be `0`.

### REQ-DEATH-02: Monster Death Spawns Corpse

`do_monster_death()` MUST spawn the corpse item at the death tile via the overlay system (same mechanism as `spawn_splash`). Tile capacity (10 things) MUST be checked; if full, the spawn is silently skipped.

**Scenario: Corpse appears** — GIVEN a monster with `look_corpse=3058`, WHEN it dies, THEN item 3058 appears at its position.

**Scenario: No corpse for zero ID** — GIVEN `look_corpse=0`, WHEN the monster dies, THEN no item is placed.

**Scenario: Tile cap respected** — GIVEN death tile has 10 things, WHEN spawning corpse, THEN the spawn MUST be silently skipped (no panic, no error).

### REQ-DEATH-03: Monster Loot on Death

After corpse spawn, `do_monster_death()` MUST call `roll_drops()` and place each rolled item via `spawn_loot()` at the death tile.

**Scenario: Loot from rolled drops** — GIVEN a monster whose roll produces `[(2671, 1)]`, WHEN `spawn_loot` runs, THEN item 2671 appears on the ground.

**Scenario: Empty loot table** — GIVEN a monster with no loot, WHEN `roll_drops()` returns empty, THEN no ground items spawn.

**Scenario: Tile cap per loot** — GIVEN near-capacity tile, WHEN loot items would exceed the cap, THEN overflow items are silently dropped.

### REQ-DEATH-04: Player Death Spawns Corpse

`do_death()` for a player MUST spawn item `3058` (human corpse) at the death tile, following the same tile-cap check.

**Scenario: Human corpse** — GIVEN a player dies, WHEN `do_death` processes, THEN item 3058 appears at the death position.

### REQ-DEATH-05: Player Loot Pool

On player death, the system MUST collect all items from the player's inventory (8 equipment slots) and the contents of their backpack as the loot pool. Each item becomes a guaranteed drop.

**Scenario: Inventory items in pool** — GIVEN a player with item in slot 3, WHEN they die, THEN that item is in the loot pool.

**Scenario: Backpack contents in pool** — GIVEN a player with items inside their backpack, WHEN they die, THEN those items are in the loot pool.

**Scenario: Empty inventory** — GIVEN a player with no items, WHEN they die, THEN the loot pool is empty; only the corpse spawns.

### REQ-DEATH-06: Player Loot on Ground

Player loot items MUST be placed on the death tile via `spawn_loot` (same mechanism as monster loot).

**Scenario: Items dropped** — GIVEN a player with 3 items in the loot pool, WHEN `spawn_loot` runs, THEN 3 items appear on the ground at the death position.

### REQ-DEATH-07: Loot Positioning

Loot items MUST be placed in a loose pattern around the corpse — on the corpse tile or adjacent walkable tiles, using the same positioning logic as existing `spawn_loot`.

**Scenario: Items near corpse** — GIVEN a corpse at (10,10,7), WHEN loot drops, THEN items appear at (10,10,7) or adjacent walkable tiles.
