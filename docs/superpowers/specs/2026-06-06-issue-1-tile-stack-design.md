# ISSUE-1 — Per-tile item stack + correct stackpos (design)

Date: 2026-06-06
Status: approved, ready for implementation plan
Tracks: `docs/superpowers/specs/2026-06-06-known-issues.md` (ISSUE-1)

## Problem

The world model stores **one item per tile** (the ground) and the wire encoder
sends only that item. Walls, wall-borders, columns, and every object stacked above
the ground are dropped, so the rendered world is "flat" and wrong. Wall collision
is also absent because walls are never represented server-side.

Evidence (confirmed in code):

- `crates/world/src/map.rs:19` — `ground: HashMap<(u16,u16,u8), u16>`, one client id
  per coordinate.
- `crates/world/src/map.rs:37` — build reads `tile.items.first()` only; `items[1..]`
  are dropped.
- `crates/protocol/src/map_description.rs:149` — `add_item` writes exactly one
  `[u16 clientId][u8 0xFF]` per tile; no per-tile item loop.
- `crates/protocol/src/map_description.rs:19` — `GroundSource::ground -> Option<u16>`
  cannot express a stack.
- `crates/protocol/src/walk.rs:61` — `creature_move(old, 1, new)` hardcodes
  stackpos = 1.

## Scope

Render + collision **and** correct creature stackpos (chosen over the doc's
render-only minimum). Floor changes / underground (z>=8) remain out of scope —
that is ISSUE-2.

## Reference (TFS, byte-faithful targets)

- `reference/tfs/src/protocolgame.cpp:583` `GetTileDescription` — wire order per tile:
  `env(u16) → ground → top items (always-on-top, sorted by alwaysOnTopOrder) →
  creatures (reverse) → down items (the rest)`, capped at **10 things total**.
- `reference/tfs/src/tile.cpp:1190` `getClientIndexOfCreature` — a creature's
  stackpos = `(ground ? 1 : 0) + topItemCount`, then count visible creatures before
  it. For a lone player this is `ground + topItemCount`.
- `reference/tfs/src/protocolgame.cpp:2599` `sendMoveCreature` — `0x6D` carries the
  creature's old stackpos (the `< 10` form).
- `reference/tfs/src/itemloader.h:156` `FLAG_ALWAYSONTOP = 1 << 13`.
- `reference/tfs/src/itemloader.h:133` `ITEM_ATTR_TOPORDER = 0x2B`.
- `ITEM_GROUP_GROUND = 1` (itemgroup_t; the OTB item node kind).

## Design

### 1. `formats::otb` — expose item-type attributes

Extend `ItemType` so the world can classify items:

- `always_on_top: bool` ← `flags & FLAG_ALWAYSONTOP (1 << 13) != 0`.
- `top_order: u8` ← parse `ITEM_ATTR_TOPORDER (0x2B)` (currently skipped via the
  `_ => r.skip(len)` arm). Defaults to 0 when absent.
- Ground classification uses the existing `group` field (`group == 1`); no new
  field needed, optionally a `is_ground()` helper.

### 2. `world::StaticMap` — per-tile stack model

Replace `ground: HashMap<(u16,u16,u8), u16>` with
`tiles: HashMap<(u16,u16,u8), TileStack>`:

```rust
struct TileStack {
    /// Wire-ordered client ids: [ground, ...top (by top_order), ...down], cap 10.
    items: Vec<u16>,
    /// Split point: items[0..pre_creature_len] render before a creature
    /// (ground + top items); items[pre_creature_len..] render after (down items).
    pre_creature_len: usize,
}
```

Build (`from_formats`): for each `MapItem`, resolve its `ItemType` and classify:

- `group == ITEM_GROUP_GROUND` → ground bucket (expect one; first wins).
- else `always_on_top` → top bucket.
- else → down bucket.

Sort the top bucket by `top_order` ascending (stable, preserving file order on
ties — mirrors `addThing` insert-before-equal). Concatenate
`ground ++ top ++ down`, map server_id → client_id, truncate to 10. Set
`pre_creature_len = min(ground_count + top_count, 10)`.

`blocked` (already per-item via `FLAG_BLOCK_SOLID`) and `spawn` are unchanged.
`is_walkable` reads the new `tiles` map for the ground-present check (a tile is
walkable iff it has a stack with a ground item and is not block-solid).

### 3. Trait — `GroundSource` → `TileSource`

Rename and broaden:

```rust
pub trait TileSource {
    /// Wire-ordered tile contents at a coordinate, split around the creature
    /// slot. None when the tile has no ground (empty / out of bounds).
    fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>>;

    /// The stackpos a creature occupies on this tile (= pre_creature_len, capped
    /// at 10). 1 for a plain ground-only tile.
    fn creature_stackpos(&self, x: i32, y: i32, z: i32) -> u8;
}

pub struct TileSlices<'a> {
    pub pre_creature: &'a [u16],  // ground + top items
    pub post_creature: &'a [u16], // down items
}
```

Rename all `GroundSource` references (`map_description.rs`, `walk.rs`, test stubs)
to `TileSource`. Test stubs implement the two methods trivially.

### 4. `map_description` — loop the stack

In `get_map_description`, on a real tile:

1. flush any open skip run, write `env u16 (0x0000)`.
2. write each id in `pre_creature` via `add_item`, counting things up to 10.
3. splice matching creature bytes (same as today, after the pre-creature items).
4. if count < 10, write `post_creature` ids via `add_item` until count reaches 10.

The skip-encoding (skip starts at -1, persists across floors, `[0xFF][0xFF]` flush
at 0xFE) is untouched. A tile is "real" iff `src.tile(...)` is `Some`.

### 5. `walk.rs` — real stackpos

`walk_update` computes the old stackpos from the source:

```rust
let mut out = creature_move(old, src.creature_stackpos(ox, oy, oz), new);
```

The directional slices already render correctly once the encoder loops the stack.

### 6. Tests (TDD)

- **otb**: an item node with `FLAG_ALWAYSONTOP` and an `ITEM_ATTR_TOPORDER` record
  parses `always_on_top == true` and the right `top_order`.
- **world**: a tile with ground + two top items (different `top_order`) + one down
  item produces `items` in `[ground, top(low), top(high), down]` order;
  `pre_creature_len == 3`; an 11-item tile truncates to 10.
- **map_description**: a multi-item tile with a creature round-trips through the
  existing decoder with the creature spliced between top and down items; a tile
  that would exceed 10 things stops at 10.
- **walk**: `walk_update` emits `0x6D` with the computed stackpos (e.g. 2 on a
  ground+1-top tile), and 1 on a plain ground tile (regression).
- **regression**: a ground-only map encodes byte-identically to today.

## Out of scope

- Floor changes / underground (z>=8) viewport stacking — ISSUE-2.
- Item runtime mutation (the map stays immutable / `Arc`).
- Stackable item counts, animation phases, splash/magic-field special cases — the
  wire form stays `[u16 clientId][u8 0xFF]` per item.
