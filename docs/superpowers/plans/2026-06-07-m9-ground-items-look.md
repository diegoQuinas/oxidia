# M9 — Ground Items, Stacks, Look-at — Implementation Plan

> **For agentic workers:** This plan follows the project's **spike-first** workflow
> (reaffirmed 2026-06-07), NOT TDD. Implement each layer, keep `cargo build` +
> `cargo clippy` green, then the user validates live in OTClient. **Tests are
> written LAST (Task 8), only after the user confirms the feature works.** Do not
> write feature tests before the live-validation gate (Task 7). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Examine (look-at) any tile thing — items with TFS-faithful
name/description/weight and real stack counts, plus creatures — over the world
actor, replying with `0xB4 MESSAGE_INFO_DESCR`.

**Architecture:** Look-at logic lives in the `world` actor (`do_look`), which
owns both the tile stacks (`StaticMap`) and the creatures (player registry).
Item metadata comes from `items.xml` (loaded into an `ItemMeta` catalog on
`StaticMap`); stack counts come from a new `OTBM_ATTR_COUNT` parse. The protocol
crate stays pure wire (parse `0x8C`/`0x8D`, encode `0xB4`).

**Tech Stack:** Rust 1.96, edition 2024, `#![forbid(unsafe_code)]`, tokio actor,
roxmltree (items.xml), OTBM/OTB binary parsers.

**Reference design:** `docs/superpowers/specs/2026-06-07-m9-ground-items-look-design.md`.
TFS oracle lines (read while implementing): inbound `parseLookAt`
`protocolgame.cpp:908`; `MESSAGE_INFO_DESCR = 22` `const.h:191`; item text
`item.cpp::getDescription:893` + `getNameDescription:1536` + weight `:1577`;
player text `player.cpp:85`; GM debug `data/scripts/eventcallbacks/player/default_onLook.lua`.

---

## File map

| File | Responsibility | Change |
|---|---|---|
| `crates/formats/src/items_xml.rs` | items.xml attrs | add name/article/plural/description/weight/show_count to `ItemXmlAttrs` + accessors |
| `crates/formats/src/otb.rs` | items.otb item types | add `FLAG_PICKUPABLE` + `is_pickupable()` |
| `crates/formats/src/otbm.rs` | map tile/item parse | add `MapItem.count` + `OTBM_ATTR_COUNT` attr loop |
| `crates/protocol/src/look.rs` (new) | look wire | `parse_look`, `parse_look_battle`, `info_descr` |
| `crates/protocol/src/lib.rs` | module list | `pub mod look;` |
| `crates/world/src/map.rs` | tile stacks + catalog | `ItemMeta`, `TileStack.server_ids`, count threading, `load_item_metadata`, accessors |
| `crates/world/src/game.rs` | world actor | `Command::LookAt`/`LookBattle`, `do_look`, text builders, gamemaster plumbing |
| `crates/server/src/game_service.rs` | session reader | dispatch `0x8C`/`0x8D`; set `initial.gamemaster` |
| `crates/server/src/main.rs` | boot wiring | call `static_map.load_item_metadata(&items, &items_xml)` |

---

## Task 1: Item metadata in `items.xml` + `is_pickupable`

**Files:**
- Modify: `crates/formats/src/items_xml.rs`
- Modify: `crates/formats/src/otb.rs`

- [ ] **Step 1: Extend `ItemXmlAttrs` and add accessors**

In `crates/formats/src/items_xml.rs`, replace the `ItemXmlAttrs` struct (currently
only `floor_change`) with:

```rust
/// Per-item attributes loaded from `items.xml`, keyed by server id.
#[derive(Debug, Clone, Default)]
pub struct ItemXmlAttrs {
    pub floor_change: FloorChange,
    /// Display name (`<item name=…>`); empty if absent.
    pub name: String,
    /// Indefinite article, `"a"` / `"an"` (`<item article=…>`).
    pub article: String,
    /// Plural name (`<item plural=…>`), used for stackable count > 1.
    pub plural: String,
    /// Look description (`<attribute key="description">`).
    pub description: String,
    /// Weight in **hundredths of an oz** (`<attribute key="weight">`).
    pub weight: u32,
    /// Whether the count prefixes the name for stacks (`showcount`, default true).
    pub show_count: bool,
}
```

Because `show_count` must default to `true` (not `false`), the `Default` derive
is wrong for it. Replace the derive with a manual impl:

```rust
impl Default for ItemXmlAttrs {
    fn default() -> Self {
        Self {
            floor_change: FloorChange::NONE,
            name: String::new(),
            article: String::new(),
            plural: String::new(),
            description: String::new(),
            weight: 0,
            show_count: true,
        }
    }
}
```

Remove `#[derive(... Default)]` from the struct (keep `Debug, Clone`).

- [ ] **Step 2: Parse the new attributes in `parse_items_xml`**

In `parse_items_xml`, the loop builds one `attrs` per `<item>` then merges into
`by_server_id`. Replace the body so it reads the element attributes and the
`<attribute>` children, and merges ALL fields (not just floor_change):

```rust
    for item in doc.descendants().filter(|n| n.has_tag_name("item")) {
        let ids = item_id_range(&item);
        if ids.is_empty() {
            continue;
        }
        let mut attrs = ItemXmlAttrs::default();
        // Element attributes on <item …>.
        if let Some(v) = item.attribute("name") { attrs.name = v.to_string(); }
        if let Some(v) = item.attribute("article") { attrs.article = v.to_string(); }
        if let Some(v) = item.attribute("plural") { attrs.plural = v.to_string(); }
        // <attribute key=… value=…> children.
        for attr in item.children().filter(|n| n.has_tag_name("attribute")) {
            let key = attr.attribute("key").unwrap_or("");
            let value = attr.attribute("value").unwrap_or("");
            if key.eq_ignore_ascii_case("floorchange") {
                if let Some(fc) = FloorChange::from_xml_value(value) {
                    attrs.floor_change.insert(fc);
                }
            } else if key.eq_ignore_ascii_case("description") {
                attrs.description = value.to_string();
            } else if key.eq_ignore_ascii_case("weight") {
                attrs.weight = value.parse::<u32>().unwrap_or(0);
            } else if key.eq_ignore_ascii_case("showcount") {
                attrs.show_count = !value.eq_ignore_ascii_case("0")
                    && !value.eq_ignore_ascii_case("false");
            }
        }
        for id in ids {
            let entry = by_server_id.entry(id).or_default();
            entry.floor_change.insert(attrs.floor_change);
            // Element/text metadata: overwrite only when this <item> set it,
            // so a later id-range entry doesn't clobber a specific one with "".
            if !attrs.name.is_empty() { entry.name = attrs.name.clone(); }
            if !attrs.article.is_empty() { entry.article = attrs.article.clone(); }
            if !attrs.plural.is_empty() { entry.plural = attrs.plural.clone(); }
            if !attrs.description.is_empty() { entry.description = attrs.description.clone(); }
            if attrs.weight != 0 { entry.weight = attrs.weight; }
            entry.show_count = attrs.show_count;
        }
    }
```

Note: `by_server_id.entry(id).or_default()` now uses the manual `Default`
(`show_count = true`), which is correct.

- [ ] **Step 3: Add `FLAG_PICKUPABLE` + `is_pickupable()` in otb.rs**

In `crates/formats/src/otb.rs`, near the other flag constants (around line 29),
add:

```rust
/// `FLAG_PICKUPABLE` (bit 5) — the item can be picked up; look-at shows weight
/// only for pickupable items (TFS `item.cpp:1499`).
const FLAG_PICKUPABLE: u32 = 1 << 5;
```

In the `impl ItemType` block, next to `is_stackable`, add:

```rust
    /// `FLAG_PICKUPABLE` — look-at shows a weight line only for pickupable items.
    pub fn is_pickupable(&self) -> bool {
        self.flags & FLAG_PICKUPABLE != 0
    }
```

- [ ] **Step 4: Build + clippy**

Run: `cargo build -p formats && cargo clippy -p formats --all-targets -- -D warnings`
Expected: clean (existing tests still compile — no `ItemType`/`ItemXmlAttrs`
literal added required fields except `ItemXmlAttrs`, which is only constructed via
`Default` in non-test code; the one literal in `items_xml.rs` tests constructs
`ItemType`, not `ItemXmlAttrs`, so it is unaffected).

- [ ] **Step 5: Commit**

```bash
git add crates/formats/src/items_xml.rs crates/formats/src/otb.rs
git commit -m "feat(formats): load item name/article/plural/description/weight + is_pickupable"
```

---

## Task 2: OTBM stack count parsing

**Files:**
- Modify: `crates/formats/src/otbm.rs`

- [ ] **Step 1: Add `count` to `MapItem`**

In `crates/formats/src/otbm.rs`, extend the struct (around line 82):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapItem {
    /// Server item id.
    pub id: u16,
    /// Stack count / subtype from `OTBM_ATTR_COUNT` (None if absent).
    pub count: Option<u8>,
    /// Items contained within (for containers); empty otherwise.
    pub contents: Vec<MapItem>,
}
```

- [ ] **Step 2: Add the `OTBM_ATTR_COUNT` constant**

Near the other OTBM attr constants (around line 33):

```rust
const OTBM_ATTR_COUNT: u8 = 15;
```

- [ ] **Step 3: Read attributes in `parse_item`**

Replace `parse_item` (around line 234) so it reads the item-node attribute bytes
after the id, capturing `OTBM_ATTR_COUNT`. Stop at the first unknown attribute
(map ground items carry at most COUNT plus a few well-known ids):

```rust
/// Parse an OTBM_ITEM node: leading u16 id, then attributes (we capture COUNT),
/// then contained items as child nodes. Attribute parsing stops at the first
/// unknown tag — map stack items carry COUNT and a small set of known attrs.
fn parse_item(node: &Node) -> Result<MapItem, FormatError> {
    let mut r = PropReader::new(&node.props);
    let id = r.read_u16()?;
    let mut count = None;
    while r.remaining() > 0 {
        let attr = r.read_u8()?;
        match attr {
            OTBM_ATTR_COUNT => count = Some(r.read_u8()?),
            OTBM_ATTR_ACTION_ID | OTBM_ATTR_UNIQUE_ID | OTBM_ATTR_DEPOT_ID
            | OTBM_ATTR_RUNE_CHARGES | OTBM_ATTR_CHARGES => { r.read_u16()?; }
            OTBM_ATTR_TELE_DEST => { r.skip(5)?; } // x u16, y u16, z u8
            OTBM_ATTR_DURATION | OTBM_ATTR_WRITTENDATE => { r.read_u32()?; }
            OTBM_ATTR_DECAYING_STATE => { r.read_u8()?; }
            OTBM_ATTR_TEXT | OTBM_ATTR_DESC | OTBM_ATTR_WRITTENBY => { r.read_string()?; }
            _ => break, // unknown attr: stop (leftover bytes ignored, as before)
        }
    }
    let mut contents = Vec::with_capacity(node.children.len());
    for child in &node.children {
        if child.kind != OTBM_ITEM {
            return Err(FormatError::InvalidNode { what: "unknown contained item node" });
        }
        contents.push(parse_item(child)?);
    }
    Ok(MapItem { id, count, contents })
}
```

Add the referenced constants near `OTBM_ATTR_COUNT`:

```rust
const OTBM_ATTR_ACTION_ID: u8 = 4;
const OTBM_ATTR_UNIQUE_ID: u8 = 5;
const OTBM_ATTR_TEXT: u8 = 6;
const OTBM_ATTR_DESC: u8 = 7;
const OTBM_ATTR_TELE_DEST: u8 = 8;
const OTBM_ATTR_DEPOT_ID: u8 = 10;
const OTBM_ATTR_RUNE_CHARGES: u8 = 12;
const OTBM_ATTR_DURATION: u8 = 16;
const OTBM_ATTR_DECAYING_STATE: u8 = 17;
const OTBM_ATTR_WRITTENDATE: u8 = 18;
const OTBM_ATTR_WRITTENBY: u8 = 19;
const OTBM_ATTR_CHARGES: u8 = 22;
```

If `PropReader` has no `skip`, replace `r.skip(5)?` with five `r.read_u8()?;`.

- [ ] **Step 4: Fix the inline ground-item `MapItem` construction**

In `parse_tile` (around line 216) the inline ground item now needs the `count`
field:

```rust
            OTBM_ATTR_ITEM => items.push(MapItem { id: r.read_u16()?, count: None, contents: vec![] }),
```

The compiler will also flag any `MapItem { … }` literal in `otbm.rs` tests — add
`count: None` to each.

- [ ] **Step 5: Build + clippy**

Run: `cargo build -p formats && cargo clippy -p formats --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/formats/src/otbm.rs
git commit -m "feat(formats): parse OTBM_ATTR_COUNT into MapItem.count"
```

---

## Task 3: Protocol `look` module

**Files:**
- Create: `crates/protocol/src/look.rs`
- Modify: `crates/protocol/src/lib.rs`

- [ ] **Step 1: Create the module**

Create `crates/protocol/src/look.rs`:

```rust
//! Look-at (examine) wire forms.
//!
//! Inbound `0x8C` (look at a tile thing) and `0x8D` (look in battle list); the
//! outbound reply is a `0xB4` text message of type `MESSAGE_INFO_DESCR`. The
//! text itself is assembled by `world` (it needs item metadata) — this module is
//! pure wire. Refs: `protocolgame.cpp:908` (parseLookAt), `:916`
//! (parseLookInBattleList), `const.h:191` (`MESSAGE_INFO_DESCR = 22`).

use crate::message::{MessageReader, MessageWriter};

/// TFS `MESSAGE_INFO_DESCR = 22` (`const.h:191`): green look-description message.
pub const MSG_INFO_DESCR: u8 = 22;

/// Parse inbound `0x8C` body (everything after the opcode byte):
/// `[x u16][y u16][z u8][spriteId u16, ignored][stackpos u8]`.
/// Returns `(x, y, z, stackpos)`, or `None` if the body is malformed.
pub fn parse_look(body: &[u8]) -> Option<(u16, u16, u8, u8)> {
    let mut r = MessageReader::new(body);
    let x = r.read_u16().ok()?;
    let y = r.read_u16().ok()?;
    let z = r.read_u8().ok()?;
    let _sprite = r.read_u16().ok()?; // spriteId, ignored (server resolves by stackpos)
    let stackpos = r.read_u8().ok()?;
    Some((x, y, z, stackpos))
}

/// Parse inbound `0x8D` body: `[creatureId u32]`. Returns the id or `None`.
pub fn parse_look_battle(body: &[u8]) -> Option<u32> {
    MessageReader::new(body).read_u32().ok()
}

/// Encode an outbound `0xB4 MESSAGE_INFO_DESCR` text message:
/// `[0xB4][22][u16 len][bytes]`. The string is Latin-1 bytes; over-255 is
/// truncated (documented divergence, same as chat).
pub fn info_descr(text: &[u8]) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(0xB4);
    w.write_u8(MSG_INFO_DESCR);
    w.write_string(&text[..text.len().min(255)]);
    w.into_bytes()
}
```

If `MessageReader` lacks `read_u8`/`read_u16`/`read_u32` returning `Result`, match
the existing API used in `game_login.rs` (it calls `.read_u8()? `). Confirm by
reading `crates/protocol/src/message.rs`.

- [ ] **Step 2: Register the module**

In `crates/protocol/src/lib.rs`, add alongside the other `pub mod` lines:

```rust
pub mod look;
```

- [ ] **Step 3: Build + clippy**

Run: `cargo build -p protocol && cargo clippy -p protocol --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/protocol/src/look.rs crates/protocol/src/lib.rs
git commit -m "feat(protocol): look module — parse 0x8C/0x8D, encode 0xB4 info_descr"
```

---

## Task 4: `StaticMap` item catalog, server-ids, count threading

**Files:**
- Modify: `crates/world/src/map.rs`

- [ ] **Step 1: Add `ItemMeta` and import `ItemsXml`**

At the top of `crates/world/src/map.rs`, extend the formats import:

```rust
use formats::items_xml::{FloorChange, ItemsXml};
```

Add the metadata struct near the top (after the imports):

```rust
/// Look-at metadata for one item type, combining `items.xml` text with the
/// `items.otb` flags. Keyed by server id in `StaticMap::item_meta`.
#[derive(Debug, Clone, Default)]
pub struct ItemMeta {
    pub name: String,
    pub article: String,
    pub plural: String,
    pub description: String,
    /// Weight in hundredths of an oz.
    pub weight: u32,
    pub show_count: bool,
    pub stackable: bool,
    pub pickupable: bool,
}
```

- [ ] **Step 2: Carry `server_ids` on `TileStack`**

Replace the `TileStack` struct (around line 23):

```rust
struct TileStack {
    /// `[ground, ...top items (by top_order), ...down items]`, capped at 10.
    items: Vec<WireItem>,
    /// Server ids parallel to `items` (same order/length) for look-at metadata.
    server_ids: Vec<u16>,
    /// OTBM stack counts parallel to `items` (None when unspecified). Drives the
    /// look-at text ("You see 50 gold coins.") for stackable items.
    counts: Vec<Option<u8>>,
    /// `items[..pre_creature_len]` render below a creature (ground + top items).
    pre_creature_len: usize,
}
```

- [ ] **Step 3: Thread count into `wire_item`**

Replace `wire_item` (around line 34) to take the parsed count:

```rust
/// Resolve an `items.otb` entry + its OTBM stack count into the wire form,
/// mirroring TFS `NetworkMessage::addItem`: stackable items carry a count byte,
/// splash/fluid a fluid-type byte, animated items a phase byte.
fn wire_item(it: &formats::otb::ItemType, count: Option<u8>) -> WireItem {
    let subtype = if it.is_stackable() {
        Some(count.unwrap_or(1).max(1)) // map stacks default to 1 when unspecified
    } else if it.is_fluid_or_splash() {
        Some(count.unwrap_or(0))
    } else {
        None
    };
    WireItem { client_id: it.client_id, subtype, animated: it.is_animated() }
}
```

- [ ] **Step 4: Populate `server_ids` + count in the build loop**

In `from_formats_with_spawn`, update the per-tile build. The ground/top/down
gather (lines 92-105) must also record the server id, and call `wire_item` with
`mi.count`. Replace that block:

```rust
            let mut ground: Option<(WireItem, u16, Option<u8>)> = None;
            let mut top: Vec<(u8, WireItem, u16, Option<u8>)> = Vec::new(); // (top_order, item, sid, count)
            let mut down: Vec<(WireItem, u16, Option<u8>)> = Vec::new();
            for (i, mi) in tile.items.iter().enumerate() {
                let Some(it) = by_id.get(&mi.id) else { continue };
                let wi = wire_item(it, mi.count);
                if i == 0 {
                    ground = Some((wi, mi.id, mi.count));
                } else if it.always_on_top {
                    top.push((it.top_order, wi, mi.id, mi.count));
                } else {
                    down.push((wi, mi.id, mi.count));
                }
            }

            if let Some((ground_item, ground_sid, ground_count)) = ground {
                top.sort_by_key(|(order, _, _, _)| *order); // stable: file order on ties
                let mut items: Vec<WireItem> = Vec::with_capacity(1 + top.len() + down.len());
                let mut server_ids: Vec<u16> = Vec::with_capacity(items.capacity());
                let mut counts: Vec<Option<u8>> = Vec::with_capacity(items.capacity());
                items.push(ground_item);
                server_ids.push(ground_sid);
                counts.push(ground_count);
                for (_, wi, sid, c) in &top { items.push(*wi); server_ids.push(*sid); counts.push(*c); }
                let pre_creature_len = items.len().min(MAX_TILE_THINGS);
                for (wi, sid, c) in &down { items.push(*wi); server_ids.push(*sid); counts.push(*c); }
                items.truncate(MAX_TILE_THINGS);
                server_ids.truncate(MAX_TILE_THINGS);
                counts.truncate(MAX_TILE_THINGS);
                tiles.insert((tile.x, tile.y, tile.z), TileStack { items, server_ids, counts, pre_creature_len });
            }
```

- [ ] **Step 5: Add the `item_meta` field + initialise it empty**

Add to the `StaticMap` struct (around line 50):

```rust
    /// Look-at metadata by server id; empty until `load_item_metadata` runs.
    item_meta: HashMap<u16, ItemMeta>,
```

In the struct literal returned by `from_formats_with_spawn` (around line 163),
add `item_meta: HashMap::new(),`:

```rust
        Self { tiles, blocked, floor_change, tile_height, protection_zone, spawn, item_meta: HashMap::new() }
```

- [ ] **Step 6: Add `load_item_metadata` + look-at accessors**

Add to the main `impl StaticMap` block:

```rust
    /// Populate the look-at metadata catalog from items.otb (flags) + items.xml
    /// (name/description/weight). Call once at boot, after construction. Tests
    /// that exercise look-at call this explicitly with a small fixture.
    pub fn load_item_metadata(&mut self, otb: &ItemsOtb, xml: &ItemsXml) {
        for it in &otb.items {
            let x = xml.attrs(it.server_id);
            self.item_meta.insert(it.server_id, ItemMeta {
                name: x.map(|a| a.name.clone()).unwrap_or_default(),
                article: x.map(|a| a.article.clone()).unwrap_or_default(),
                plural: x.map(|a| a.plural.clone()).unwrap_or_default(),
                description: x.map(|a| a.description.clone()).unwrap_or_default(),
                weight: x.map(|a| a.weight).unwrap_or(0),
                show_count: x.map(|a| a.show_count).unwrap_or(true),
                stackable: it.is_stackable(),
                pickupable: it.is_pickupable(),
            });
        }
    }

    /// Look-at metadata for `server_id`, or `None` if not catalogued.
    pub fn item_meta(&self, server_id: u16) -> Option<&ItemMeta> {
        self.item_meta.get(&server_id)
    }

    /// Number of wire things (items only) stacked on a tile (0 if no tile).
    pub fn tile_thing_count(&self, pos: Position) -> usize {
        self.tiles.get(&(pos.x, pos.y, pos.z)).map_or(0, |st| st.items.len())
    }

    /// How many of a tile's items render below a creature (ground + top items).
    pub fn tile_pre_creature_len(&self, pos: Position) -> usize {
        self.tiles.get(&(pos.x, pos.y, pos.z)).map_or(0, |st| st.pre_creature_len)
    }

    /// The server id of the item at index `idx` in a tile's stack, or `None`.
    pub fn tile_item_server_id(&self, pos: Position, idx: usize) -> Option<u16> {
        self.tiles.get(&(pos.x, pos.y, pos.z)).and_then(|st| st.server_ids.get(idx).copied())
    }

    /// The OTBM stack count of the item at index `idx` (None if unspecified).
    pub fn tile_item_count(&self, pos: Position, idx: usize) -> Option<u8> {
        self.tiles.get(&(pos.x, pos.y, pos.z)).and_then(|st| st.counts.get(idx).copied().flatten())
    }
```

- [ ] **Step 7: Build + clippy**

Run: `cargo build -p world && cargo clippy -p world --all-targets -- -D warnings`
Expected: clean. (No `TileStack` literal exists outside `map.rs`; `wire_item`'s
new arg is internal.)

- [ ] **Step 8: Commit**

```bash
git add crates/world/src/map.rs
git commit -m "feat(world): item-meta catalog + per-tile server ids + OTBM count threading"
```

---

## Task 5: `do_look` in the world actor + gamemaster plumbing

**Files:**
- Modify: `crates/world/src/game.rs`

- [ ] **Step 1: Add `gamemaster` to `InitialState` and `PlayerState`**

In `InitialState` (around line 76), add:

```rust
    /// `true` if the session authenticated as a gamemaster (look-at debug info).
    pub gamemaster: bool,
```

In `PlayerState` (around line 107), add:

```rust
    /// Gamemaster flag from login; gates look-at debug (item id + position).
    gamemaster: bool,
```

The compiler will flag all 12 `InitialState { … }` and 4 `PlayerState { … }`
construction sites. Add `gamemaster: false` to every TEST literal; in the live
`login` handler set `gamemaster: initial.gamemaster` on the `PlayerState` it
inserts (find the `self.players.insert(id, PlayerState { … })` in the login
handler and read `gamemaster` from the `initial`/`InitialState` in scope).

- [ ] **Step 2: Add the look commands + handle arms**

In `enum Command` (around line 1038), add:

```rust
    /// Client `0x8C`: look at the thing at `(x,y,z)` stackpos `stackpos`.
    LookAt { id: u32, x: u16, y: u16, z: u8, stackpos: u8 },
    /// Client `0x8D`: look at a creature in the battle list by id.
    LookBattle { id: u32, target_id: u32 },
```

In `handle` (around line 285), add arms:

```rust
            Command::LookAt { id, x, y, z, stackpos } => self.do_look(id, x, y, z, stackpos),
            Command::LookBattle { id, target_id } => self.do_look_battle(id, target_id),
```

- [ ] **Step 3: Add the `WorldHandle` methods**

In `impl WorldHandle` (after `request_outfit`, around line 1113):

```rust
    /// Look at a tile thing (`0x8C`). Fire-and-forget; the world pushes `0xB4`.
    pub async fn look(&self, id: u32, x: u16, y: u16, z: u8, stackpos: u8) {
        let _ = self.tx.send(Command::LookAt { id, x, y, z, stackpos }).await;
    }

    /// Look at a creature in the battle list (`0x8D`). Fire-and-forget.
    pub async fn look_battle(&self, id: u32, target_id: u32) {
        let _ = self.tx.send(Command::LookBattle { id, target_id }).await;
    }
```

- [ ] **Step 4: Add the `MSG_INFO_DESCR` constant + a push helper**

Near `MSG_STATUS_SMALL` (around line 42):

```rust
/// TFS `MESSAGE_INFO_DESCR = 22`: green look-description message (`const.h:191`).
const MSG_INFO_DESCR: u8 = 22;
```

Near `push_status_message` (around line 605), add:

```rust
    /// Push a `0xB4 MESSAGE_INFO_DESCR` look description to a single player.
    fn push_info_descr(&mut self, id: u32, text: &str) {
        let bytes = text.as_bytes();
        let mut w = protocol::message::MessageWriter::new();
        w.write_u8(0xB4);
        w.write_u8(MSG_INFO_DESCR);
        w.write_string(&bytes[..bytes.len().min(255)]);
        self.push(id, w.into_bytes());
    }
```

- [ ] **Step 5: Implement `do_look`, `do_look_battle`, and the text builders**

Add these methods to `impl Game` (place them after `do_say`, before the combat
section). Read `item.cpp:893/1536/1577` and `player.cpp:85` while porting the
exact punctuation.

```rust
    /// Handle `0x8C` look-at. Resolve the thing at `(x,y,z)` stackpos, build the
    /// TFS "You see …" text, and push `0xB4`. Mirrors `Game::playerLookAt`
    /// (game.cpp:3100): resolve thing, canSee check, distance, describe.
    fn do_look(&mut self, id: u32, x: u16, y: u16, z: u8, stackpos: u8) {
        let Some(looker) = self.players.get(&id) else { return };
        let looker_pos = looker.position;
        let gm = looker.gamemaster;
        let pos = Position::new(x, y, z);

        // Visibility: the looker must be able to see the tile.
        if !Self::can_see(looker_pos, pos) {
            return;
        }

        let pre = self.map.tile_pre_creature_len(pos);
        let creatures = self.creatures_on(pos); // ids, arrival order (ground+top first)

        // STACKPOS_LOOK resolution under the ≤1-creature invariant (co-occupancy
        // only on stair landings). Order: ground+top items, creature(s), down items.
        let sp = stackpos as usize;
        let text = if sp < pre {
            // Item below the creature slot.
            self.describe_tile_item(pos, sp, looker_pos, gm)
        } else if !creatures.is_empty() && sp < pre + creatures.len() {
            // The creature occupying this tile.
            let target = creatures[sp - pre];
            self.describe_creature(id, target, gm)
        } else {
            // A down item (shift the index past the creature slot(s)).
            let idx = sp.saturating_sub(creatures.len());
            self.describe_tile_item(pos, idx, looker_pos, gm)
        };

        if let Some(text) = text {
            self.push_info_descr(id, &text);
        }
    }

    /// Handle `0x8D` look-in-battle-list: describe a creature by id.
    fn do_look_battle(&mut self, id: u32, target_id: u32) {
        let Some(looker) = self.players.get(&id) else { return };
        let Some(target) = self.players.get(&target_id) else { return };
        if !Self::can_see(looker.position, target.position) {
            return;
        }
        let gm = looker.gamemaster;
        if let Some(text) = self.describe_creature(id, target_id, gm) {
            self.push_info_descr(id, &text);
        }
    }

    /// Ids of creatures standing on `pos`, ground+top arrival order. Mirrors the
    /// stackpos ordering used by `creature_stackpos_on`.
    fn creatures_on(&self, pos: Position) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .players
            .iter()
            .filter(|(_, p)| p.position == pos)
            .map(|(&pid, _)| pid)
            .collect();
        ids.sort_unstable(); // deterministic; refine to arrival order if needed
        ids
    }

    /// Build the "You see …" text for the tile item at stack index `idx`.
    /// `None` if the tile / index has no catalogued item. Ports
    /// `item.cpp::getDescription` (plain-item subset) + `getNameDescription`.
    fn describe_tile_item(
        &self,
        pos: Position,
        idx: usize,
        looker_pos: Position,
        gm: bool,
    ) -> Option<String> {
        let sid = self.map.tile_item_server_id(pos, idx)?;
        let meta = self.map.item_meta(sid)?;
        // Real OTBM stack count (parsed in Task 2); 1 when unspecified. Drives
        // both the "<count> <plural>" name and the total-weight line.
        let count = u32::from(self.map.tile_item_count(pos, idx).unwrap_or(1).max(1));

        // lookDistance = Chebyshev; +15 across floors (game.cpp:3123).
        let mut dist = (i32::from(looker_pos.x) - i32::from(pos.x))
            .abs()
            .max((i32::from(looker_pos.y) - i32::from(pos.y)).abs());
        if looker_pos.z != pos.z {
            dist += 15;
        }

        // Name description (getNameDescription:1536).
        let mut s = String::from("You see ");
        if meta.stackable && count > 1 && meta.show_count {
            s.push_str(&format!("{} {}", count, meta.plural));
        } else if !meta.name.is_empty() {
            if !meta.article.is_empty() {
                s.push_str(&meta.article);
                s.push(' ');
            }
            s.push_str(&meta.name);
        } else {
            s.push_str(&format!("an item of type {}", sid));
        }
        s.push('.');

        // Weight + description only when adjacent (lookDistance <= 1, item.cpp:1496).
        if dist <= 1 {
            if meta.pickupable && meta.weight != 0 {
                let total = meta.weight * count; // hundredths of oz
                let plural = meta.stackable && count > 1;
                s.push('\n');
                s.push_str(if plural { "They weigh " } else { "It weighs " });
                s.push_str(&format!("{}.{:02} oz.", total / 100, total % 100));
            }
            if !meta.description.is_empty() {
                s.push('\n');
                s.push_str(&meta.description);
            }
        }

        if gm {
            s.push_str(&format!("\nItem ID: {}", sid));
            s.push_str(&format!("\nPosition: {}, {}, {}", pos.x, pos.y, pos.z));
        }
        Some(s)
    }

    /// Build the "You see …" text for a creature. Ports `player.cpp:85`
    /// (faithful subset: name, level, vocation; no party/mana/IP).
    fn describe_creature(&self, looker_id: u32, target_id: u32, gm: bool) -> Option<String> {
        let target = self.players.get(&target_id)?;
        let is_self = looker_id == target_id;
        let mut s = String::from("You see ");
        if is_self {
            s.push_str("yourself. You have no vocation.");
        } else {
            s.push_str(&target.name);
            s.push_str(" (Level 1)."); // real level lands with M14
            s.push_str(if target.sex == 0 { " She" } else { " He" });
            s.push_str(" has no vocation.");
        }
        if gm {
            let p = target.position;
            s.push_str(&format!("\nPosition: {}, {}, {}", p.x, p.y, p.z));
        }
        Some(s)
    }
```

If `Position::new` is not in scope inside `game.rs`, it already is (used widely);
confirm the import line.

- [ ] **Step 6: Build + clippy**

Run: `cargo build -p world && cargo clippy -p world --all-targets -- -D warnings`
Expected: clean once all 12 `InitialState` + 4 `PlayerState` literals carry
`gamemaster`.

- [ ] **Step 7: Commit**

```bash
git add crates/world/src/game.rs
git commit -m "feat(world): do_look + look-battle, TFS item/creature describe, gamemaster gate"
```

---

## Task 6: Wire `0x8C`/`0x8D` and the boot catalog

**Files:**
- Modify: `crates/server/src/game_service.rs`
- Modify: `crates/server/src/main.rs`

- [ ] **Step 1: Set `gamemaster` on `InitialState`**

In `crates/server/src/game_service.rs`, both `InitialState` build paths must carry
the login flag. Easiest: after the `let initial = match &save { … };` block (around
line 230), insert:

```rust
    let mut initial = initial;
    initial.gamemaster = req.gamemaster;
```

Also add `gamemaster: false` to the two `InitialState { … }` literals in this file
(the `None =>` default at ~line 222 and the one in `player_save_to_initial` at
~line 39) so they compile; the line above then overrides with the real value.

- [ ] **Step 2: Dispatch `0x8C` and `0x8D` in `reader_loop`**

In `reader_loop`, before the `let Some((direction, is_turn)) = opcode_action(opcode)`
fallthrough (around line 382), add:

```rust
            // 0x8C — client look-at (parseLookAt). Body: [pos][spriteId u16][stackpos u8].
            if opcode == 0x8C {
                if let Some((x, y, z, stackpos)) = protocol::look::parse_look(&payload[1..]) {
                    world.look(id, x, y, z, stackpos).await;
                }
                continue;
            }
            // 0x8D — client look-in-battle-list. Body: [creatureId u32].
            if opcode == 0x8D {
                if let Some(target_id) = protocol::look::parse_look_battle(&payload[1..]) {
                    world.look_battle(id, target_id).await;
                }
                continue;
            }
```

(If `protocol` is imported under a shorter alias in this file, match it; otherwise
the fully-qualified `protocol::look::…` path works.)

- [ ] **Step 3: Load the item-metadata catalog at boot**

In `crates/server/src/main.rs`, the map is built around the existing
`merge_items_xml` call (line 74). The `static_map` is currently
`Arc::new(StaticMap::from_formats_with_spawn(…))`. Build it mutable, load the
catalog, then wrap in `Arc`:

```rust
    let mut static_map = world::map::StaticMap::from_formats_with_spawn(
        &map, &items, world_cfg_town /* keep the existing args */,
    );
    static_map.load_item_metadata(&items, &items_xml);
    let static_map = std::sync::Arc::new(static_map);
```

Match the EXACT existing argument list of `from_formats_with_spawn` at that call
site (do not change which town is passed). `items` and `items_xml` are already in
scope from the existing load+merge.

- [ ] **Step 4: Build + clippy (workspace)**

Run: `cargo build && cargo clippy --all-targets -- -D warnings`
Expected: clean across the workspace.

- [ ] **Step 5: Commit**

```bash
git add crates/server/src/game_service.rs crates/server/src/main.rs
git commit -m "feat(server): dispatch 0x8C/0x8D look-at; load item-meta catalog at boot"
```

---

## Task 7: LIVE VALIDATION GATE (user spike) 🎮

**Do not write tests yet.** Hand off to the user to validate in real OTClient.

- [ ] **Step 1: Run the server**

Run: `RUST_LOG=info cargo run -p server -- config/server.toml`
Expected: boots clean, listens on 7171/7172, logs the loaded item count.

- [ ] **Step 2: User validates in OTClient Redemption**

Ask the user to confirm each:
1. Right-click / look on a **ground item** near the temple → green "You see a
   `<name>`." appears.
2. Standing **adjacent** to that item → the weight line ("It weighs X.YY oz.")
   and the item description appear; standing **2+ tiles away** → they don't.
3. Look on a **second player** → "You see `<name>` (Level 1). He has no
   vocation." (and "She" for a female character).
4. Look on **yourself** → "You see yourself. You have no vocation."
5. With a **gamemaster** login → item look shows the extra "Item ID" + "Position"
   lines; a normal login does not.

- [ ] **Step 3: Triage**

If anything is wrong, fix it and re-run Step 1-2 before proceeding. Only when the
user confirms it all works do we move to Task 8. **Record the user's "it works"
explicitly.**

---

## Task 8: Tests (AFTER live validation)

Only start this task once the user has confirmed Task 7 passes. Cover the wire
and the text logic. (Live testing cannot catch regressions — this locks it in.)

- [ ] **Step 1: formats tests**

Add to `crates/formats/src/items_xml.rs` tests: a doc with `name`/`article`/
`plural` element attrs + `description`/`weight`/`showcount` children parses into
the expected `ItemXmlAttrs` (weight stays in hundredths; `show_count` defaults
true when absent, false for `value="0"`). Add to `crates/formats/src/otbm.rs`
tests: an `OTBM_ITEM` node with an `OTBM_ATTR_COUNT` byte yields
`MapItem.count == Some(n)`, and an unknown trailing attr does not panic.

- [ ] **Step 2: protocol look tests**

In `crates/protocol/src/look.rs`, add `#[cfg(test)]`: `parse_look` round-trips
`(x, y, z, stackpos)` and skips the 2-byte spriteId; `parse_look_battle` reads
the id; `info_descr(b"hi")` emits `[0xB4, 22, 0x02, 0x00, b'h', b'i']`.

- [ ] **Step 3: world `do_look` tests**

In `crates/world/src/game.rs` tests, build a `StaticMap`, call
`load_item_metadata` with a small `ItemsXml`, register a player, and assert the
pushed `0xB4` payload decodes to the expected string for: a ground item (article+
name, '.'); weight+description present at distance ≤1 and absent at ≥2;
non-pickupable / weight-0 → no weight line; look at another player →
"`<name>` (Level 1). He has no vocation." (and "She" after setting `sex = 0`);
a **stackable** item with OTBM count 50 → "You see 50 `<plural>`." and the weight
line uses `weight × 50`; self-look → "yourself"; gamemaster looker → "Item ID" +
"Position" appended;
non-gamemaster → not appended; out-of-view tile → no packet pushed.

- [ ] **Step 4: Full gate**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: whole workspace green, clippy clean, `#![forbid(unsafe_code)]` intact.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "test(m9): ground-item metadata, OTBM count, look wire, do_look text"
```

- [ ] **Step 6: Update PROGRESS.md**

Add the M9 row state `✅ done` and an M9 plan section noting the live acceptance,
mirroring the M6.1/M7 sections.

---

## Self-review notes

- **Spec coverage:** A (metadata) → Tasks 1-2; B (catalog, count, do_look) →
  Tasks 4-5; C (protocol wire) → Task 3; D (wiring + gamemaster) → Task 6;
  `0x8D` → Tasks 3/5/6; live acceptance → Task 7; tests → Task 8. All covered.
- **Real counts in look text (locked decision):** Task 4 stores per-tile OTBM
  counts (`TileStack.counts`) and `describe_tile_item` reads them via
  `tile_item_count`, so a gold pile reads "You see 50 gold coins." and weighs
  `weight × count`. This honours the "parsear ahora" scope decision — counts flow
  to BOTH the wire render and the examine text.
- **Stackpos resolution** assumes the ≤1-creature-per-tile common case; stair
  co-occupancy is handled by indexing `creatures_on`. Lenient on out-of-range
  stackpos (drops silently), matching the "no packet on unresolved" behaviour.
