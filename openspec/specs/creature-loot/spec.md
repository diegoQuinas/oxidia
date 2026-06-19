# Creature Loot Specification

## Purpose

Define loot table structs, XML parsing from `<loot>` blocks in monster definitions, and chance-based rolling so creatures drop items with independent per-item probability on death.

## Requirements

### REQ-LOOT-01: Loot Table Struct

The system MUST define `MonsterDrop { item_id: u16, chance: f64, count_max: u16 }`. Chance is a raw value `0.0..=1.0` (parsed from XML as `chance/N/100`).

**Scenario: Struct created** — GIVEN `MonsterDrop { item_id: 2666, chance: 0.5, count_max: 1 }`, WHEN inspected, THEN fields match construction.

**Scenario: Zero chance valid** — GIVEN `chance = 0.0`, WHEN `MonsterDrop` is constructed, THEN it MUST be valid (effectively a no-drop entry).

### REQ-LOOT-02: XML Loot Block Parsing

The system MUST parse `<loot>` sections from monster XML. Each `<item id="X" chance="Y" countmax="Z"/>` MUST produce a `MonsterDrop`. Missing `countmax` defaults to 1. `<loot>` is optional — its absence produces an empty loot vector.

**Scenario: Single item** — GIVEN `<item id="2671" chance="25" countmax="3"/>`, WHEN parsed, THEN `MonsterDrop { item_id: 2671, chance: 0.25, count_max: 3 }`.

**Scenario: No loot block** — GIVEN a monster XML without `<loot>`, WHEN parsed, THEN `MonsterType.loot` MUST be empty.

**Scenario: Missing countmax** — GIVEN `<item id="2666" chance="10"/>`, WHEN parsed, THEN `count_max` MUST be 1.

**Scenario: Multiple items** — GIVEN 3 `<item>` tags in `<loot>`, WHEN parsed, THEN the loot table MUST contain 3 entries.

### REQ-LOOT-03: Chance-Based Drop Rolling

`roll_drops(&[MonsterDrop]) -> Vec<(u16, u8)>` MUST evaluate each entry independently by its chance value. Successful entries produce a random count `1..=count_max`. Failed entries MUST NOT appear.

**Scenario: Independent per-item rolls** — GIVEN two items both at chance=0.5, WHEN rolling, THEN each has an independent 50% probability of inclusion.

**Scenario: All succeed** — GIVEN 3 items at chance=1.0, WHEN rolling, THEN all 3 MUST appear.

**Scenario: All fail** — GIVEN an item at chance=0.0, WHEN rolling, THEN it MUST NOT appear.

**Scenario: Count range respected** — GIVEN an item with count_max=5, WHEN rolled successfully, THEN count MUST be in `[1, 5]`.

### REQ-LOOT-04: Loot Table on MonsterType

`MonsterType` MUST carry `loot: Vec<MonsterDrop>`, populated by XML parsing and defaulting to empty.

**Scenario: Default empty** — GIVEN `MonsterType::default()`, WHEN inspecting `loot`, THEN it MUST be empty.
