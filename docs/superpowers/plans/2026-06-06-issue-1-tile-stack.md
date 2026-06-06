# ISSUE-1 — Per-tile Item Stack + Correct Stackpos Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render the full per-tile item stack (walls, borders, objects) and send the player's real stackpos, replacing the ground-only world model.

**Architecture:** Store each tile as an ordered `TileStack` of client ids split around the creature slot (ground+top items, then down items), mirroring TFS `GetTileDescription`. Broaden the `GroundSource` trait to `TileSource` so the encoder loops the stack; compute the creature's stackpos from the stack for `0x6D`.

**Tech Stack:** Rust workspace (`formats`, `protocol`, `world`, `server` crates), protocol 10.98, TFS 1.4.2 + OTClient as byte references.

**Design doc:** `docs/superpowers/specs/2026-06-06-issue-1-tile-stack-design.md`

**Key reference facts (verified in code):**
- Wire order per tile (`reference/tfs/src/protocolgame.cpp:583` `GetTileDescription`):
  `env(u16=0) → ground → top items (always-on-top, sorted by alwaysOnTopOrder) → creatures (reverse) → down items`, capped at **10 things total**.
- Per item (`reference/tfs/src/networkmessage.cpp:82` `addItem`): `[clientId u16][0xFF]` for plain items (extra bytes only for stackable/splash/animation — out of scope).
- Client (`reference/otclient .../protocolgameparse.cpp:3890` `setTileDescription`): reads things until `peekU16() >= 0xff00`. Client ids are always `< 0xff00`; the skip marker `[skip][0xFF]` peeks `>= 0xff00`.
- Creature stackpos (`reference/tfs/src/tile.cpp:1190` `getClientIndexOfCreature`): `(ground?1:0) + topItemCount` for a lone player.
- `FLAG_ALWAYSONTOP = 1 << 13` (`itemloader.h:156`); `ITEM_ATTR_TOPORDER = 0x2B` (`itemloader.h:133`); `ITEM_GROUP_GROUND = 1`.

**Design decision (deviation from spec, noted):** Ground is identified as `tile.items[0]` (the existing, empirically-valid invariant — ground tiles already render correctly), NOT by `group == 1`. The remaining items `[1..]` are classified into top (always-on-top) / down by their `items.otb` attributes. This keeps the change minimal and preserves current behavior for ground; group-based ground detection is left out to avoid churning test fixtures that use `group: 0`.

---

## File Structure

- `crates/formats/src/otb.rs` — add `always_on_top` + `top_order` to `ItemType`; parse `ITEM_ATTR_TOPORDER`.
- `crates/protocol/src/map_description.rs` — replace `GroundSource` with `TileSource` + `TileSlices`; loop the stack in the encoder; extend test decoder.
- `crates/protocol/src/walk.rs` — `TileSource` rename; real stackpos in `walk_update`.
- `crates/world/src/map.rs` — `TileStack` model; classify items in `from_formats`; implement `TileSource`; update `is_walkable`.
- `crates/server/src/game_service.rs` — rename the `map_description::GroundSource` bound to `TileSource`.

---

## Task 1: Expose item-type attributes in `formats::otb`

**Files:**
- Modify: `crates/formats/src/otb.rs`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/formats/src/otb.rs`:

```rust
    /// An always-on-top item with a TOPORDER attribute parses both fields.
    #[test]
    fn parses_always_on_top_and_top_order() {
        let mut v = vec![0x00, 0x00, 0x00, 0x00]; // identifier
        v.push(0xFE); // root START
        v.push(0x00); // root type
        v.extend_from_slice(&0u32.to_le_bytes()); // root flags
        v.push(ROOT_ATTR_VERSION);
        v.extend_from_slice(&140u16.to_le_bytes());
        v.extend_from_slice(&3u32.to_le_bytes()); // major
        v.extend_from_slice(&57u32.to_le_bytes()); // minor
        v.extend_from_slice(&0u32.to_le_bytes()); // build
        v.extend_from_slice(&[0u8; 128]); // CSDVersion

        v.push(0xFE); // item START
        v.push(0x05); // group (non-ground, arbitrary)
        v.extend_from_slice(&(1u32 << 13).to_le_bytes()); // flags = FLAG_ALWAYSONTOP
        v.push(ITEM_ATTR_SERVER_ID);
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&200u16.to_le_bytes());
        v.push(ITEM_ATTR_CLIENT_ID);
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&1059u16.to_le_bytes());
        v.push(ITEM_ATTR_TOPORDER);
        v.extend_from_slice(&1u16.to_le_bytes()); // len = 1
        v.push(3u8); // top_order = 3
        v.push(0xFF); // item END

        v.push(0xFF); // root END

        let otb = parse(&v).unwrap();
        assert_eq!(otb.items.len(), 1);
        let it = &otb.items[0];
        assert!(it.always_on_top, "FLAG_ALWAYSONTOP set");
        assert_eq!(it.top_order, 3);
        assert!(!it.is_ground());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p formats parses_always_on_top_and_top_order`
Expected: FAIL — `no field 'always_on_top' on type 'ItemType'` / `no method 'is_ground'`.

- [ ] **Step 3: Add fields, the attribute constant, the flag, and parsing**

In `crates/formats/src/otb.rs`, add the constants next to the existing `ITEM_ATTR_*`:

```rust
/// `ITEM_ATTR_TOPORDER` — a single byte: the always-on-top draw order.
const ITEM_ATTR_TOP_ORDER: u8 = 0x2B;

/// `FLAG_ALWAYSONTOP` bit of the per-item flags word (`itemflags_t`).
const FLAG_ALWAYS_ON_TOP: u32 = 1 << 13;

/// `ITEM_GROUP_GROUND` (`itemgroup_t`) — a ground-tile item.
const ITEM_GROUP_GROUND: u8 = 1;
```

Extend the `ItemType` struct (keep `derive(Debug, Clone, PartialEq, Eq)`):

```rust
pub struct ItemType {
    /// `itemgroup_t` (the node type byte).
    pub group: u8,
    /// Item behaviour flags (`itemflags_t` bitset).
    pub flags: u32,
    /// Server-side item id.
    pub server_id: u16,
    /// Client-side (sprite) item id.
    pub client_id: u16,
    /// `FLAG_ALWAYSONTOP` — renders above creatures; ordered by `top_order`.
    pub always_on_top: bool,
    /// `ITEM_ATTR_TOPORDER` — draw order among always-on-top items (0 if absent).
    pub top_order: u8,
}

impl ItemType {
    /// True if this item is a ground tile (`itemgroup_t == ITEM_GROUP_GROUND`).
    pub fn is_ground(&self) -> bool {
        self.group == ITEM_GROUP_GROUND
    }
}
```

Rewrite `parse_item` to read TOPORDER and compute `always_on_top`:

```rust
fn parse_item(node: &Node) -> Result<ItemType, FormatError> {
    let mut r = PropReader::new(&node.props);
    let flags = r.read_u32()?;
    let mut server_id = 0;
    let mut client_id = 0;
    let mut top_order = 0u8;
    while r.remaining() > 0 {
        let attr = r.read_u8()?;
        let len = r.read_u16()? as usize;
        match attr {
            ITEM_ATTR_SERVER_ID => server_id = r.read_u16()?,
            ITEM_ATTR_CLIENT_ID => client_id = r.read_u16()?,
            ITEM_ATTR_TOP_ORDER => top_order = r.read_u8()?,
            _ => r.skip(len)?,
        }
    }
    Ok(ItemType {
        group: node.kind,
        flags,
        server_id,
        client_id,
        always_on_top: flags & FLAG_ALWAYS_ON_TOP != 0,
        top_order,
    })
}
```

- [ ] **Step 4: Fix the existing literal in `parses_version_and_one_item`**

The existing assertion compares the whole struct. Update it:

```rust
        assert_eq!(
            otb.items[0],
            ItemType {
                group: 0x01,
                flags: 0x80,
                server_id: 100,
                client_id: 4526,
                always_on_top: false,
                top_order: 0,
            }
        );
```

- [ ] **Step 5: Run the formats tests**

Run: `cargo test -p formats`
Expected: PASS (all, including the new test and the real-otb test if present).

- [ ] **Step 6: Commit**

```bash
git add crates/formats/src/otb.rs
git commit -m "feat(formats): parse always-on-top flag and top-order from items.otb"
```

---

## Task 2: Broaden the encoder trait to a per-tile stack (`protocol`)

This task keeps the `protocol` crate self-consistent: the trait change forces the
encoder loop and both consumers (`map_description.rs`, `walk.rs`) to change
together. `cargo test -p protocol` is green at the end of Step 6.

**Files:**
- Modify: `crates/protocol/src/map_description.rs`
- Modify: `crates/protocol/src/walk.rs`

- [ ] **Step 1: Replace the trait with `TileSource` + `TileSlices`**

In `crates/protocol/src/map_description.rs`, replace the `GroundSource` trait
(lines ~17-21) with:

```rust
/// Wire-ordered tile contents, split around the creature slot. `pre_creature`
/// is the ground + always-on-top items (rendered below a creature);
/// `post_creature` is the remaining "down" items (rendered above).
pub struct TileSlices<'a> {
    pub pre_creature: &'a [u16],
    pub post_creature: &'a [u16],
}

/// Provides the full item stack at a world coordinate. `tile` returns `None`
/// when the tile has no ground (empty / out of bounds).
pub trait TileSource {
    /// The tile's client-id stack, split around the creature slot.
    fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>>;

    /// The stackpos a creature occupies on this tile (`pre_creature` length,
    /// capped at 10). 1 on a plain ground-only tile.
    fn creature_stackpos(&self, x: i32, y: i32, z: i32) -> u8;
}
```

- [ ] **Step 2: Loop the stack in `get_map_description`**

In the same file, replace the `match src.ground(wx, wy, nz)` block inside
`get_map_description` (the `Some`/`None` arms) with:

```rust
                match src.tile(wx, wy, nz) {
                    Some(slices) => {
                        if skip >= 0 {
                            w.write_u8(skip as u8);
                            w.write_u8(0xFF);
                        }
                        skip = 0;
                        w.write_u16(0x0000); // environmental effects placeholder
                        let mut things: u8 = 0;
                        for &client_id in slices.pre_creature {
                            if things == 10 {
                                break;
                            }
                            add_item(w, client_id);
                            things += 1;
                        }
                        for c in creatures {
                            if i32::from(c.x) == wx
                                && i32::from(c.y) == wy
                                && i32::from(c.z) == nz
                            {
                                w.write_bytes(&c.bytes);
                                things = things.saturating_add(1);
                            }
                        }
                        if things < 10 {
                            for &client_id in slices.post_creature {
                                if things == 10 {
                                    break;
                                }
                                add_item(w, client_id);
                                things += 1;
                            }
                        }
                    }
                    None => {
                        if skip == 0xFE {
                            w.write_u8(0xFF);
                            w.write_u8(0xFF);
                            skip = -1;
                        } else {
                            skip += 1;
                        }
                    }
                }
```

Update the doc comment on `get_map_description` to say it writes the full tile
stack (ground + top items, creatures, then down items) capped at 10 things.

- [ ] **Step 3: Update the test stub and decoder to handle stacks**

In the `tests` module of `map_description.rs`, replace `MapStub` and its impl with
a stack-backed stub, and make the decoders loop things per tile.

Replace the stub:

```rust
    /// Maps a coordinate to its full wire-ordered stack (pre_creature first).
    struct MapStub {
        stacks: HashMap<(i32, i32, i32), (Vec<u16>, usize)>,
    }
    impl MapStub {
        fn ground_only(m: HashMap<(i32, i32, i32), u16>) -> Self {
            let stacks = m
                .into_iter()
                .map(|(k, cid)| (k, (vec![cid], 1usize)))
                .collect();
            Self { stacks }
        }
    }
    impl TileSource for MapStub {
        fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>> {
            self.stacks.get(&(x, y, z)).map(|(items, pre)| TileSlices {
                pre_creature: &items[..*pre],
                post_creature: &items[*pre..],
            })
        }
        fn creature_stackpos(&self, x: i32, y: i32, z: i32) -> u8 {
            self.stacks.get(&(x, y, z)).map_or(1, |(_, pre)| *pre as u8)
        }
    }
```

Change `decode_stream` to collect a `Vec<u16>` per tile by looping things until a
`>= 0xFF00` marker. Replace its body's per-tile read with:

```rust
    fn decode_stream(bytes: &[u8], center: Center) -> HashMap<(i32, i32, i32), Vec<u16>> {
        assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);
        let mut p = 6usize; // skip opcode + u16 x + u16 y + u8 z
        let anchor_x = center.x as i32 - ANCHOR_DX;
        let anchor_y = center.y as i32 - ANCHOR_DY;
        let floor_size = VIEWPORT_WIDTH * VIEWPORT_HEIGHT;
        let total = 8 * floor_size;
        let mut found = HashMap::new();
        let mut skip = 0i32;
        let mut g_idx = 0i32;
        while g_idx < total {
            if skip == 0 {
                let peek = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
                if peek >= 0xFF00 {
                    skip = i32::from(peek & 0x00FF);
                    p += 2;
                } else {
                    // Tile: [env u16] then things until the next >= 0xFF00 marker.
                    assert_eq!(peek, 0x0000, "tile env effects at {p}");
                    p += 2;
                    let mut ids = Vec::new();
                    loop {
                        let v = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
                        if v >= 0xFF00 {
                            skip = i32::from(v & 0x00FF);
                            p += 2;
                            break;
                        }
                        // plain item: [clientId u16][0xFF mark]
                        assert_eq!(bytes[p + 2], MARK_UNMARKED, "item mark at {}", p + 2);
                        ids.push(v);
                        p += 3;
                    }
                    let fi = g_idx / floor_size;
                    let nz = 7 - fi;
                    let offset = center.z as i32 - nz;
                    let t = g_idx % floor_size;
                    let nx = t / VIEWPORT_HEIGHT;
                    let ny = t % VIEWPORT_HEIGHT;
                    found.insert((anchor_x + nx + offset, anchor_y + ny + offset, nz), ids);
                }
            } else {
                skip -= 1;
            }
            g_idx += 1;
        }
        found
    }
```

Apply the same loop change to `decode_slice` (it returns
`HashMap<(i32,i32,i32), Vec<u16>>` now):

```rust
    fn decode_slice(
        bytes: &[u8],
        anchor_x: i32,
        anchor_y: i32,
        center_z: i32,
        width: i32,
        height: i32,
    ) -> std::collections::HashMap<(i32, i32, i32), Vec<u16>> {
        let floor_size = width * height;
        let total = 8 * floor_size;
        let mut found = std::collections::HashMap::new();
        let mut p = 0usize;
        let mut skip = 0i32;
        let mut g_idx = 0i32;
        while g_idx < total {
            if skip == 0 {
                let peek = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
                if peek >= 0xFF00 {
                    skip = i32::from(peek & 0x00FF);
                    p += 2;
                } else {
                    p += 2; // env u16
                    let mut ids = Vec::new();
                    loop {
                        let v = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
                        if v >= 0xFF00 {
                            skip = i32::from(v & 0x00FF);
                            p += 2;
                            break;
                        }
                        ids.push(v);
                        p += 3;
                    }
                    let fi = g_idx / floor_size;
                    let nz = 7 - fi;
                    let offset = center_z - nz;
                    let t = g_idx % floor_size;
                    let nx = t / height;
                    let ny = t % height;
                    found.insert((anchor_x + nx + offset, anchor_y + ny + offset, nz), ids);
                }
            } else {
                skip -= 1;
            }
            g_idx += 1;
        }
        found
    }
```

Update the existing tests that build/read stubs:
- `empty_map_is_only_skip_flushes`: `MapStub::ground_only(HashMap::new())`; `found.is_empty()` unchanged.
- `single_ground_tile_at_center_round_trips`: `MapStub::ground_only(m)`; assert `found.get(&(1000,1000,7)) == Some(&vec![4526u16])` and `found.len() == 1`.
- `creature_bytes_follow_the_center_ground_item`: `MapStub::ground_only(m)` (uses `find_subsequence`, unchanged otherwise).
- `header_carries_center_position`: `MapStub::ground_only(HashMap::new())`.
- `slice_round_trips_a_single_row`: `MapStub::ground_only(m)`; assert `found.get(&(1005,994,z)) == Some(&vec![4526u16])`.

- [ ] **Step 4: Update `walk.rs` to the new trait + real stackpos**

In `crates/protocol/src/walk.rs`:

Change the import (line 5):

```rust
use crate::map_description::{self, PlacedCreature, TileSource};
```

Change the `walk_update` signature and the `creature_move` call:

```rust
pub fn walk_update<S: TileSource>(
    old: (u16, u16, u8),
    new: (u16, u16, u8),
    src: &S,
    creatures: &[PlacedCreature],
) -> Vec<u8> {
    let stackpos = src.creature_stackpos(i32::from(old.0), i32::from(old.1), i32::from(old.2));
    let mut out = creature_move(old, stackpos, new);
    // ... rest of the function is unchanged ...
```

Update the `MapStub` in `walk.rs` tests to implement `TileSource`:

```rust
    struct MapStub(HashMap<(i32, i32, i32), u16>);
    impl TileSource for MapStub {
        fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>> {
            // Stub returns a one-item ground stack; pre/post share storage via leak-free slices.
            self.0.get(&(x, y, z)).map(|cid| TileSlices {
                pre_creature: std::slice::from_ref(cid),
                post_creature: &[],
            })
        }
        fn creature_stackpos(&self, _x: i32, _y: i32, _z: i32) -> u8 {
            1
        }
    }
```

Add `TileSlices` to the import in the test module (or use the full path):

```rust
    use crate::map_description::TileSlices;
```

The existing `walk.rs` tests (`east_step_emits_move_then_east_slice`,
`northeast_step_emits_both_slices`) keep working: empty stub → `creature_stackpos`
returns 1, identical to before.

- [ ] **Step 5: Run the protocol tests**

Run: `cargo test -p protocol`
Expected: PASS (all existing tests, now stack-aware).

- [ ] **Step 6: Commit**

```bash
git add crates/protocol/src/map_description.rs crates/protocol/src/walk.rs
git commit -m "feat(protocol): loop per-tile item stack in map encoder (TileSource)"
```

- [ ] **Step 7: Write the failing multi-item round-trip test**

Add to the `tests` module of `map_description.rs`:

```rust
    #[test]
    fn multi_item_tile_round_trips_in_wire_order() {
        let center = Center { x: 1000, y: 1000, z: 7 };
        // pre_creature = [ground, top_a, top_b]; post_creature = [down].
        let mut stacks = HashMap::new();
        stacks.insert((1000, 1000, 7), (vec![4526u16, 1000u16, 1001u16, 2000u16], 3usize));
        let stub = MapStub { stacks };
        let bytes = encode(center, &stub, &[]);
        let found = decode_stream(&bytes, center);
        assert_eq!(found.get(&(1000, 1000, 7)), Some(&vec![4526, 1000, 1001, 2000]));
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn tile_stack_caps_at_ten_things() {
        let center = Center { x: 1000, y: 1000, z: 7 };
        let ids: Vec<u16> = (1..=12u16).collect(); // 12 items
        let mut stacks = HashMap::new();
        stacks.insert((1000, 1000, 7), (ids, 12usize));
        let stub = MapStub { stacks };
        let bytes = encode(center, &stub, &[]);
        let found = decode_stream(&bytes, center);
        assert_eq!(found.get(&(1000, 1000, 7)).map(|v| v.len()), Some(10));
    }

    #[test]
    fn creature_splices_between_top_and_down_items() {
        let center = Center { x: 1000, y: 1000, z: 7 };
        // pre_creature = [ground, top]; post_creature = [down].
        let mut stacks = HashMap::new();
        stacks.insert((1000, 1000, 7), (vec![4526u16, 1059u16, 2000u16], 2usize));
        let stub = MapStub { stacks };
        let creature = PlacedCreature { x: 1000, y: 1000, z: 7, bytes: vec![0x61, 0x00, 0xAA, 0xBB] };
        let bytes = encode(center, &stub, std::slice::from_ref(&creature));
        // top item (1059 = 0x0423) then mark, then creature bytes, then down item (2000 = 0x07D0).
        let top = [0x23, 0x04, 0xFF];
        let down = [0xD0, 0x07, 0xFF];
        let ti = find_subsequence(&bytes, &top).expect("top item present");
        let ci = find_subsequence(&bytes, &creature.bytes).expect("creature present");
        let di = find_subsequence(&bytes, &down).expect("down item present");
        assert!(ti < ci, "creature after top item");
        assert!(ci < di, "creature before down item");
    }
```

- [ ] **Step 8: Run the new tests**

Run: `cargo test -p protocol multi_item_tile_round_trips_in_wire_order tile_stack_caps_at_ten_things creature_splices_between_top_and_down_items`
Expected: PASS (the encoder from Step 2 already implements this).

- [ ] **Step 9: Commit**

```bash
git add crates/protocol/src/map_description.rs
git commit -m "test(protocol): cover multi-item stack, 10-thing cap, creature splice order"
```

---

## Task 3: Store the per-tile stack in `world::StaticMap`

**Files:**
- Modify: `crates/world/src/map.rs`

- [ ] **Step 1: Write the failing stack-ordering test**

Add to the `tests` module of `crates/world/src/map.rs`:

```rust
    #[test]
    fn builds_ordered_stack_with_pre_creature_split() {
        use protocol::map_description::TileSource;
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0 },
                // two always-on-top items, top_order 2 then 1 (must sort to 1,2)
                ItemType { group: 5, flags: 1 << 13, server_id: 200, client_id: 1000, always_on_top: true, top_order: 2 },
                ItemType { group: 5, flags: 1 << 13, server_id: 201, client_id: 1001, always_on_top: true, top_order: 1 },
                // a down item (not always-on-top)
                ItemType { group: 5, flags: 0, server_id: 300, client_id: 2000, always_on_top: false, top_order: 0 },
            ],
        };
        let map = OtbmMap {
            width: 100, height: 100, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![MapTile {
                x: 95, y: 117, z: 7, flags: 0, house_id: None,
                items: vec![
                    MapItem { id: 100, contents: vec![] }, // ground
                    MapItem { id: 200, contents: vec![] }, // top order 2
                    MapItem { id: 201, contents: vec![] }, // top order 1
                    MapItem { id: 300, contents: vec![] }, // down
                ],
            }],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        let slices = sm.tile(95, 117, 7).expect("tile present");
        // ground, then top items sorted by top_order (1001 before 1000).
        assert_eq!(slices.pre_creature, &[4526, 1001, 1000]);
        assert_eq!(slices.post_creature, &[2000]);
        assert_eq!(sm.creature_stackpos(95, 117, 7), 3);
        assert_eq!(sm.creature_stackpos(1, 1, 7), 1); // missing tile defaults to 1
    }

    #[test]
    fn stack_truncates_to_ten_things() {
        use protocol::map_description::TileSource;
        let mut item_defs = vec![ItemType {
            group: 1, flags: 0, server_id: 1, client_id: 5000, always_on_top: false, top_order: 0,
        }];
        let mut tile_items = vec![MapItem { id: 1, contents: vec![] }]; // ground
        for sid in 2..=12u16 {
            item_defs.push(ItemType {
                group: 5, flags: 0, server_id: sid, client_id: 6000 + sid,
                always_on_top: false, top_order: 0,
            });
            tile_items.push(MapItem { id: sid, contents: vec![] });
        }
        let items = ItemsOtb { major_version: 3, minor_version: 57, build_number: 0, items: item_defs };
        let map = OtbmMap {
            width: 100, height: 100, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![MapTile { x: 95, y: 117, z: 7, flags: 0, house_id: None, items: tile_items }],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        let slices = sm.tile(95, 117, 7).expect("tile present");
        assert_eq!(slices.pre_creature.len() + slices.post_creature.len(), 10);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p world builds_ordered_stack_with_pre_creature_split`
Expected: FAIL — `no method 'tile'` on `StaticMap` / field/struct mismatches.

- [ ] **Step 3: Replace the model and the builder**

In `crates/world/src/map.rs`, replace the imports, struct, and `from_formats`.

Update imports/header:

```rust
//! Immutable world map: a per-tile item stack + a spawn point.
//! Client ids are resolved once from items.otb (server_id -> client_id).

use std::collections::{HashMap, HashSet};

use formats::otb::ItemsOtb;
use formats::otbm::OtbmMap;
use protocol::map_description::{TileSlices, TileSource};

use crate::Position;
```

Add the stack type and replace the `ground` field:

```rust
/// Maximum things (items + creature) the client renders per tile.
const MAX_TILE_THINGS: usize = 10;

/// Wire-ordered client ids for one tile, split around the creature slot.
struct TileStack {
    /// `[ground, ...top items (by top_order), ...down items]`, capped at 10.
    items: Vec<u16>,
    /// `items[..pre_creature_len]` render below a creature (ground + top items).
    pre_creature_len: usize,
}

pub struct StaticMap {
    tiles: HashMap<(u16, u16, u8), TileStack>,
    blocked: HashSet<(u16, u16, u8)>,
    spawn: Position,
}
```

Replace `from_formats`:

```rust
    /// Build from a parsed map + item dictionary. Each tile becomes a wire-ordered
    /// stack: ground (`items[0]`), then always-on-top items sorted by `top_order`,
    /// then the remaining "down" items, capped at 10 things (TFS stackpos cap).
    pub fn from_formats(map: &OtbmMap, items: &ItemsOtb) -> Self {
        let by_id: HashMap<u16, &formats::otb::ItemType> =
            items.items.iter().map(|it| (it.server_id, it)).collect();

        let mut tiles = HashMap::new();
        let mut blocked = HashSet::new();
        for tile in &map.tiles {
            let mut ground: Option<u16> = None;
            let mut top: Vec<(u8, u16)> = Vec::new(); // (top_order, client_id)
            let mut down: Vec<u16> = Vec::new();
            for (i, mi) in tile.items.iter().enumerate() {
                let Some(it) = by_id.get(&mi.id) else { continue };
                if i == 0 {
                    ground = Some(it.client_id);
                } else if it.always_on_top {
                    top.push((it.top_order, it.client_id));
                } else {
                    down.push(it.client_id);
                }
            }

            if let Some(ground_cid) = ground {
                top.sort_by_key(|(order, _)| *order); // stable: file order on ties
                let mut stack: Vec<u16> = Vec::with_capacity(1 + top.len() + down.len());
                stack.push(ground_cid);
                stack.extend(top.iter().map(|(_, cid)| *cid));
                let pre_creature_len = stack.len().min(MAX_TILE_THINGS);
                stack.extend(down);
                stack.truncate(MAX_TILE_THINGS);
                tiles.insert((tile.x, tile.y, tile.z), TileStack { items: stack, pre_creature_len });
            }

            let solid = tile.items.iter().any(|mi| {
                by_id.get(&mi.id).is_some_and(|it| it.flags & FLAG_BLOCK_SOLID != 0)
            });
            if solid {
                blocked.insert((tile.x, tile.y, tile.z));
            }
        }

        let spawn = map
            .towns
            .first()
            .map(|t| Position::new(t.x, t.y, t.z))
            .unwrap_or(FALLBACK_SPAWN);

        Self { tiles, blocked, spawn }
    }
```

- [ ] **Step 4: Update `is_walkable` and implement `TileSource`**

Replace `is_walkable` and the `GroundSource` impl:

```rust
    /// A tile is walkable if it has a ground stack and no block-solid item.
    pub fn is_walkable(&self, pos: Position) -> bool {
        self.tiles.contains_key(&(pos.x, pos.y, pos.z))
            && !self.blocked.contains(&(pos.x, pos.y, pos.z))
    }
}

impl StaticMap {
    /// Bounds-check a world coordinate down to a `(u16, u16, u8)` tile key.
    fn key(x: i32, y: i32, z: i32) -> Option<(u16, u16, u8)> {
        if !(0..=i32::from(u16::MAX)).contains(&x)
            || !(0..=i32::from(u16::MAX)).contains(&y)
            || !(0..=i32::from(u8::MAX)).contains(&z)
        {
            return None;
        }
        Some((x as u16, y as u16, z as u8))
    }
}

impl TileSource for StaticMap {
    fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>> {
        let key = Self::key(x, y, z)?;
        let st = self.tiles.get(&key)?;
        Some(TileSlices {
            pre_creature: &st.items[..st.pre_creature_len],
            post_creature: &st.items[st.pre_creature_len..],
        })
    }

    fn creature_stackpos(&self, x: i32, y: i32, z: i32) -> u8 {
        Self::key(x, y, z)
            .and_then(|k| self.tiles.get(&k))
            .map_or(1, |st| st.pre_creature_len as u8)
    }
}
```

- [ ] **Step 5: Fix the existing `world::map` tests for the new `ItemType` fields**

The two existing fixtures (`tiny_map`, `walkability_uses_block_solid_flag`)
construct `ItemType` literals and call `sm.ground(...)`. Update them:

In `tiny_map`, add the new fields and make the ground a real ground group:

```rust
            items: vec![ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0 }],
```

In `resolves_ground_client_id_and_spawn`, replace the `sm.ground(...)` calls with
`tile(...)` checks (add `use protocol::map_description::TileSource;` at the top of
the test):

```rust
    #[test]
    fn resolves_ground_client_id_and_spawn() {
        use protocol::map_description::TileSource;
        let (map, items) = tiny_map();
        let sm = StaticMap::from_formats(&map, &items);
        assert_eq!(sm.spawn(), Position::new(95, 117, 7));
        assert_eq!(sm.tile(95, 117, 7).unwrap().pre_creature, &[4526]);
        assert!(sm.tile(0, 0, 7).is_none());
        assert!(sm.tile(-1, 0, 7).is_none());
    }
```

In `walkability_uses_block_solid_flag`, add the new fields to both `ItemType`
literals (`group: 1` for the ground, `group: 5` for the wall) and add
`always_on_top: false, top_order: 0` to each. The `is_walkable` assertions are
unchanged.

- [ ] **Step 6: Run the world tests**

Run: `cargo test -p world`
Expected: PASS (new stack tests + updated fixtures + `game.rs` walk tests).

- [ ] **Step 7: Commit**

```bash
git add crates/world/src/map.rs
git commit -m "feat(world): store per-tile item stack and serve it via TileSource"
```

---

## Task 4: Rename the trait bound in the server and verify the workspace

**Files:**
- Modify: `crates/server/src/game_service.rs:42`

- [ ] **Step 1: Rename the trait bound**

In `crates/server/src/game_service.rs`, change line 42 from
`map: &impl map_description::GroundSource,` to:

```rust
    map: &impl map_description::TileSource,
```

(`main.rs` and `game.rs` reference `StaticMap` / `is_walkable` by name only — no
change needed.)

- [ ] **Step 2: Build and test the whole workspace**

Run: `cargo test`
Expected: PASS — all crates green.

- [ ] **Step 3: Lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings. (If `pre_creature_len as u8` triggers `cast_possible_truncation`, it is bounded by `MAX_TILE_THINGS = 10` — the surrounding crate already allows the workspace lint profile; do not add a cast lint allow unless clippy flags it, then use `u8::try_from(...).unwrap_or(10)`.)

- [ ] **Step 4: Commit**

```bash
git add crates/server/src/game_service.rs
git commit -m "refactor(server): rename GroundSource bound to TileSource"
```

---

## Self-Review Notes

- **Spec coverage:** §1 otb attrs → Task 1; §2 TileStack model → Task 3; §3 trait rename/broaden → Tasks 2+4; §4 encoder loop → Task 2 Step 2; §5 walk stackpos → Task 2 Step 4; §6 tests → Tasks 1/2/3. Regression (ground-only identical) → `single_ground_tile_at_center_round_trips` + `east_step_emits_move_then_east_slice`.
- **Out of scope (unchanged):** floor changes / z>=8, item runtime mutation, stackable/animation extra bytes, group-based ground detection (uses `items[0]`).
- **Type consistency:** `TileSource::tile -> Option<TileSlices>`, `TileSource::creature_stackpos -> u8`, `TileStack { items, pre_creature_len }`, `MAX_TILE_THINGS = 10` used everywhere. `ItemType` gains `always_on_top: bool`, `top_order: u8` and method `is_ground()`.
- **Manual acceptance (after merge):** live OTClient 10.98 — walls/borders render, walls block movement, decorated-tile steps don't desync the creature (correct stackpos).
