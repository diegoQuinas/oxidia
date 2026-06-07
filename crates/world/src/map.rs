//! Immutable world map: a per-tile item stack + a spawn point.
//! Client ids are resolved once from items.otb (server_id -> client_id).

use std::collections::{HashMap, HashSet};

use formats::items_xml::{FloorChange, ItemsXml};
use formats::otb::ItemsOtb;
use formats::otbm::OtbmMap;
use protocol::map_description::{TileSlices, TileSource, WireItem};

use crate::Position;

/// Default spawn if the map has no towns (mid-map, ground level).
const FALLBACK_SPAWN: Position = Position::new(1000, 1000, 7);

/// `items.otb` `FLAG_BLOCK_SOLID` (bit 0 of the per-item flags word).
const FLAG_BLOCK_SOLID: u32 = 1 << 0;

/// Maximum things (items + creature) the client renders per tile.
const MAX_TILE_THINGS: usize = 10;

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

/// Wire-ordered items for one tile, split around the creature slot.
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

/// OTBM tile flag `PROTECTIONZONE` — bit 0 of `MapTile.flags` (iomap.h:60).
/// TFS maps this to `TILESTATE_PROTECTIONZONE` (tile.h:33) at runtime; we keep
/// the OTBM flag name here since we read it from the parsed `MapTile.flags`.
const OTBM_TILEFLAG_PROTECTIONZONE: u32 = 1 << 0;

pub struct StaticMap {
    tiles: HashMap<(u16, u16, u8), TileStack>,
    blocked: HashSet<(u16, u16, u8)>,
    /// Per-tile floor-change flags (union of the tile's items), for stairs.
    floor_change: HashMap<(u16, u16, u8), FloorChange>,
    /// Per-tile cumulative item height (count of `has_height` items).
    tile_height: HashMap<(u16, u16, u8), u8>,
    /// Tiles whose OTBM `flags & OTBM_TILEFLAG_PROTECTIONZONE != 0` — precomputed
    /// at load, mirroring the `blocked` and `floor_change` precompute pattern.
    protection_zone: HashSet<(u16, u16, u8)>,
    spawn: Position,
    /// Look-at metadata by server id; empty until `load_item_metadata` runs.
    item_meta: HashMap<u16, ItemMeta>,
}

impl StaticMap {
    /// Build from a parsed map + item dictionary, picking the spawn from the first
    /// town (or [`FALLBACK_SPAWN`]). See [`StaticMap::from_formats_with_spawn`] to
    /// target a specific town by name.
    pub fn from_formats(map: &OtbmMap, items: &ItemsOtb) -> Self {
        Self::from_formats_with_spawn(map, items, None)
    }

    /// Build from a parsed map + item dictionary. Each tile becomes a wire-ordered
    /// stack: ground (`items[0]`), then always-on-top items sorted by `top_order`,
    /// then the remaining "down" items, capped at 10 things (TFS stackpos cap).
    ///
    /// `spawn_town` names the town whose temple becomes the spawn point. When it is
    /// `None`, unknown, or the map has no towns, the spawn falls back to the first
    /// town and finally to [`FALLBACK_SPAWN`].
    pub fn from_formats_with_spawn(
        map: &OtbmMap,
        items: &ItemsOtb,
        spawn_town: Option<&str>,
    ) -> Self {
        let by_id: HashMap<u16, &formats::otb::ItemType> =
            items.items.iter().map(|it| (it.server_id, it)).collect();

        let mut tiles = HashMap::new();
        let mut blocked = HashSet::new();
        let mut floor_change = HashMap::new();
        let mut tile_height = HashMap::new();
        let mut protection_zone = HashSet::new();
        for tile in &map.tiles {
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

            let solid = tile.items.iter().any(|mi| {
                by_id.get(&mi.id).is_some_and(|it| it.flags & FLAG_BLOCK_SOLID != 0)
            });
            if solid {
                blocked.insert((tile.x, tile.y, tile.z));
            }

            // Vertical metadata: union of floor-change flags + summed item heights.
            let mut fc = FloorChange::NONE;
            let mut height: u8 = 0;
            for mi in &tile.items {
                if let Some(it) = by_id.get(&mi.id) {
                    if !it.floor_change.is_empty() {
                        fc.insert(it.floor_change);
                    }
                    if it.has_height {
                        height = height.saturating_add(1);
                    }
                }
            }
            if !fc.is_empty() {
                floor_change.insert((tile.x, tile.y, tile.z), fc);
            }
            if height > 0 {
                tile_height.insert((tile.x, tile.y, tile.z), height);
            }

            // Protection-zone flag from OTBM tile flags (iomap.h:60).
            if tile.flags & OTBM_TILEFLAG_PROTECTIONZONE != 0 {
                protection_zone.insert((tile.x, tile.y, tile.z));
            }
        }

        let named = spawn_town.and_then(|name| {
            let found = map.towns.iter().find(|t| t.name == name);
            if found.is_none() {
                tracing::warn!(town = name, "spawn_town not found in map; using first town");
            }
            found
        });
        let spawn = named
            .or_else(|| map.towns.first())
            .map(|t| Position::new(t.x, t.y, t.z))
            .unwrap_or(FALLBACK_SPAWN);

        Self { tiles, blocked, floor_change, tile_height, protection_zone, spawn, item_meta: HashMap::new() }
    }

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

    pub fn spawn(&self) -> Position {
        self.spawn
    }

    /// A tile is walkable if it has a ground stack and no block-solid item.
    pub fn is_walkable(&self, pos: Position) -> bool {
        self.tiles.contains_key(&(pos.x, pos.y, pos.z))
            && !self.blocked.contains(&(pos.x, pos.y, pos.z))
    }

    /// Does this tile have a ground item (is it a real, steppable tile)?
    pub fn has_ground(&self, pos: Position) -> bool {
        self.tiles.contains_key(&(pos.x, pos.y, pos.z))
    }

    /// Is this tile flagged block-solid?
    pub fn is_blocked(&self, pos: Position) -> bool {
        self.blocked.contains(&(pos.x, pos.y, pos.z))
    }

    /// Floor-change flags on a tile (NONE if absent / out of range).
    pub fn floor_change_at(&self, x: i32, y: i32, z: i32) -> FloorChange {
        Self::key(x, y, z)
            .and_then(|k| self.floor_change.get(&k).copied())
            .unwrap_or(FloorChange::NONE)
    }

    /// Cumulative item height on a tile (0 if none).
    pub fn tile_height(&self, pos: Position) -> u8 {
        self.tile_height.get(&(pos.x, pos.y, pos.z)).copied().unwrap_or(0)
    }

    /// Returns `true` if the tile at `pos` is flagged `OTBM_TILEFLAG_PROTECTIONZONE`
    /// (the temple/PZ tiles where combat is forbidden). Mirrors TFS
    /// `Tile::hasFlag(TILESTATE_PROTECTIONZONE)` (`combat.cpp:294-297`).
    pub fn is_protection_zone(&self, pos: Position) -> bool {
        self.protection_zone.contains(&(pos.x, pos.y, pos.z))
    }

    /// Return the respawn temple for any player. M7 respawns everyone at the single
    /// configured town temple (the `spawn` point). Per-town temple selection lands
    /// with M8 persistence once characters carry their `townId`.
    pub fn temple_for(&self, _pos: Position) -> Position {
        self.spawn
    }

    /// TFS `Tile::hasHeight(3)`: does this tile raise enough to step up onto?
    pub fn triggers_up(&self, pos: Position) -> bool {
        self.tile_height(pos) >= 3
    }

    /// Port of TFS `Tile::queryDestination` (`tile.cpp:732-811`). Given a tile
    /// carrying floor-change flags, compute the landing position one floor down
    /// (DOWN) or up (plain directional). `None` for a non-stair tile or when the
    /// coordinates leave range.
    pub fn resolve_floor_change(&self, from: Position) -> Option<Position> {
        let fc = self.floor_change_at(i32::from(from.x), i32::from(from.y), i32::from(from.z));
        if fc.is_empty() {
            return None;
        }
        let (mut dx, mut dy) = (i32::from(from.x), i32::from(from.y));

        if fc.contains(FloorChange::DOWN) {
            let dz = i32::from(from.z) + 1;
            // Adjacent-alt lookups first (two-tile-wide stairs).
            if self.floor_change_at(dx, dy - 1, dz).contains(FloorChange::SOUTH_ALT) {
                dy -= 2;
            } else if self.floor_change_at(dx - 1, dy, dz).contains(FloorChange::EAST_ALT) {
                dx -= 2;
            } else {
                let down = self.floor_change_at(dx, dy, dz);
                if down.contains(FloorChange::NORTH) { dy += 1; }
                if down.contains(FloorChange::SOUTH) { dy -= 1; }
                if down.contains(FloorChange::SOUTH_ALT) { dy -= 2; }
                if down.contains(FloorChange::EAST) { dx -= 1; }
                if down.contains(FloorChange::EAST_ALT) { dx -= 2; }
                if down.contains(FloorChange::WEST) { dx += 1; }
            }
            return to_position(dx, dy, dz);
        }

        // Plain directional (no DOWN) -> ascend one floor.
        let dz = i32::from(from.z) - 1;
        if fc.contains(FloorChange::NORTH) { dy -= 1; }
        if fc.contains(FloorChange::SOUTH) { dy += 1; }
        if fc.contains(FloorChange::EAST) { dx += 1; }
        if fc.contains(FloorChange::WEST) { dx -= 1; }
        if fc.contains(FloorChange::SOUTH_ALT) { dy += 2; }
        if fc.contains(FloorChange::EAST_ALT) { dx += 2; }
        to_position(dx, dy, dz)
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

/// Clamp an `(i32, i32, i32)` world coordinate into a `Position`, or `None` if it
/// leaves the valid range.
fn to_position(x: i32, y: i32, z: i32) -> Option<Position> {
    if (0..=i32::from(u16::MAX)).contains(&x)
        && (0..=i32::from(u16::MAX)).contains(&y)
        && (0..=i32::from(u8::MAX)).contains(&z)
    {
        Some(Position::new(x as u16, y as u16, z as u8))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use formats::otb::ItemType;
    use formats::otbm::{MapItem, MapTile, Town};

    /// Client ids of a wire-item slice, for terse stack-order assertions.
    fn cids(items: &[WireItem]) -> Vec<u16> {
        items.iter().map(|w| w.client_id).collect()
    }

    fn tiny_map() -> (OtbmMap, ItemsOtb) {
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE }],
        };
        let map = OtbmMap {
            width: 100,
            height: 100,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![MapTile {
                x: 95,
                y: 117,
                z: 7,
                flags: 0,
                house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] }],
            }],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        (map, items)
    }

    #[test]
    fn resolves_ground_client_id_and_spawn() {
        use protocol::map_description::TileSource;
        let (map, items) = tiny_map();
        let sm = StaticMap::from_formats(&map, &items);
        assert_eq!(sm.spawn(), Position::new(95, 117, 7));
        assert_eq!(cids(sm.tile(95, 117, 7).unwrap().pre_creature), vec![4526]);
        assert!(sm.tile(0, 0, 7).is_none());
        assert!(sm.tile(-1, 0, 7).is_none());
    }

    #[test]
    fn spawn_town_selects_named_temple_with_fallback() {
        let (mut map, items) = tiny_map();
        map.towns = vec![
            Town { id: 1, name: "Venore".into(), x: 95, y: 117, z: 7 },
            Town { id: 5, name: "Ab'Dendriel".into(), x: 200, y: 300, z: 7 },
        ];

        // Named town wins over the first town.
        let sm = StaticMap::from_formats_with_spawn(&map, &items, Some("Ab'Dendriel"));
        assert_eq!(sm.spawn(), Position::new(200, 300, 7));

        // Unknown name falls back to the first town.
        let sm = StaticMap::from_formats_with_spawn(&map, &items, Some("Nowhere"));
        assert_eq!(sm.spawn(), Position::new(95, 117, 7));

        // No preference falls back to the first town.
        let sm = StaticMap::from_formats_with_spawn(&map, &items, None);
        assert_eq!(sm.spawn(), Position::new(95, 117, 7));
    }

    #[test]
    fn walkability_uses_block_solid_flag() {
        // server 100 = walkable ground; server 200 = block-solid wall on the same tile.
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
                ItemType { group: 5, flags: 0x0000_0001, server_id: 200, client_id: 1059, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
            ],
        };
        let map = OtbmMap {
            width: 100, height: 100, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                // plain ground -> walkable
                MapTile { x: 95, y: 117, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
                // ground + block-solid wall -> not walkable
                MapTile { x: 96, y: 117, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, count: None, contents: vec![] }, MapItem { id: 200, count: None, contents: vec![] }] },
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        assert!(sm.is_walkable(Position::new(95, 117, 7)), "plain ground walkable");
        assert!(!sm.is_walkable(Position::new(96, 117, 7)), "block-solid wall not walkable");
        assert!(!sm.is_walkable(Position::new(1, 1, 7)), "no ground not walkable");
    }

    #[test]
    fn builds_ordered_stack_with_pre_creature_split() {
        use protocol::map_description::TileSource;
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
                ItemType { group: 5, flags: 1 << 13, server_id: 200, client_id: 1000, always_on_top: true, top_order: 2, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
                ItemType { group: 5, flags: 1 << 13, server_id: 201, client_id: 1001, always_on_top: true, top_order: 1, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
                ItemType { group: 5, flags: 0, server_id: 300, client_id: 2000, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
            ],
        };
        let map = OtbmMap {
            width: 100, height: 100, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![MapTile {
                x: 95, y: 117, z: 7, flags: 0, house_id: None,
                items: vec![
                    MapItem { id: 100, count: None, contents: vec![] },
                    MapItem { id: 200, count: None, contents: vec![] },
                    MapItem { id: 201, count: None, contents: vec![] },
                    MapItem { id: 300, count: None, contents: vec![] },
                ],
            }],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        let slices = sm.tile(95, 117, 7).expect("tile present");
        assert_eq!(cids(slices.pre_creature), vec![4526, 1001, 1000]);
        assert_eq!(cids(slices.post_creature), vec![2000]);
        assert_eq!(sm.creature_stackpos(95, 117, 7), 3);
        assert_eq!(sm.creature_stackpos(1, 1, 7), 1);
    }

    #[test]
    fn stack_truncates_to_ten_things() {
        use protocol::map_description::TileSource;
        let mut item_defs = vec![ItemType {
            group: 1, flags: 0, server_id: 1, client_id: 5000, always_on_top: false, top_order: 0,
            has_height: false, floor_change: formats::items_xml::FloorChange::NONE,
        }];
        let mut tile_items = vec![MapItem { id: 1, count: None, contents: vec![] }];
        for sid in 2..=12u16 {
            item_defs.push(ItemType {
                group: 5, flags: 0, server_id: sid, client_id: 6000 + sid,
                always_on_top: false, top_order: 0,
                has_height: false, floor_change: formats::items_xml::FloorChange::NONE,
            });
            tile_items.push(MapItem { id: sid, count: None, contents: vec![] });
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
        // Ground stays in pre_creature; the first 9 down items survive (10 total).
        assert_eq!(cids(slices.pre_creature), vec![5000]);
        assert_eq!(slices.post_creature.len(), 9);
        assert_eq!(cids(slices.post_creature), vec![6002, 6003, 6004, 6005, 6006, 6007, 6008, 6009, 6010]);
        assert_eq!(sm.creature_stackpos(95, 117, 7), 1);
    }

    #[test]
    fn more_than_ten_top_items_cap_pre_creature_at_ten() {
        use protocol::map_description::TileSource;
        let mut item_defs = vec![ItemType {
            group: 1, flags: 0, server_id: 1, client_id: 5000, always_on_top: false, top_order: 0,
            has_height: false, floor_change: formats::items_xml::FloorChange::NONE,
        }];
        let mut tile_items = vec![MapItem { id: 1, count: None, contents: vec![] }];
        for sid in 2..=12u16 {
            item_defs.push(ItemType {
                group: 5, flags: 1 << 13, server_id: sid, client_id: 6000 + sid,
                always_on_top: true, top_order: 0,
                has_height: false, floor_change: formats::items_xml::FloorChange::NONE,
            });
            tile_items.push(MapItem { id: sid, count: None, contents: vec![] });
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
        assert_eq!(slices.pre_creature.len(), 10); // ground + 9 top items
        assert_eq!(cids(slices.pre_creature), vec![5000, 6002, 6003, 6004, 6005, 6006, 6007, 6008, 6009, 6010]);
        assert!(slices.post_creature.is_empty());
        assert_eq!(sm.creature_stackpos(95, 117, 7), 10);
    }

    #[test]
    fn pz_flag_detected_on_flagged_tile() {
        // OTBM tile flag PROTECTIONZONE = 1<<0 (iomap.h:60).
        // A tile with flags & 1 == 1 must be reported as PZ; one without must not.
        let items = ItemsOtb {
            major_version: 3, minor_version: 57, build_number: 0,
            items: vec![ItemType { group: 1, flags: 0, server_id: 100, client_id: 1, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE }],
        };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                // PZ tile: flags = 1 (OTBM_TILEFLAG_PROTECTIONZONE)
                MapTile { x: 100, y: 100, z: 7, flags: 1, house_id: None,
                    items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
                // Non-PZ tile: flags = 0
                MapTile { x: 101, y: 100, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 7 }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        assert!(sm.is_protection_zone(Position::new(100, 100, 7)), "PZ tile should be PZ");
        assert!(!sm.is_protection_zone(Position::new(101, 100, 7)), "non-PZ tile should not be PZ");
        assert!(!sm.is_protection_zone(Position::new(99, 99, 7)), "absent tile should not be PZ");
    }

    #[test]
    fn temple_for_returns_spawn() {
        let (map, items) = tiny_map();
        let sm = StaticMap::from_formats(&map, &items);
        let spawn = sm.spawn();
        // M7: everyone respawns at the single town temple (the configured spawn).
        assert_eq!(sm.temple_for(Position::new(200, 200, 7)), spawn, "temple_for always returns spawn in M7");
    }

    #[test]
    fn floor_change_down_resolves_one_floor_below() {
        use formats::items_xml::FloorChange;
        // server 100 = ground; server 300 = a floorchange-down stair item.
        let items = ItemsOtb {
            major_version: 3, minor_version: 57, build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 1, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
                ItemType { group: 5, flags: 0, server_id: 300, client_id: 2, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::DOWN },
            ],
        };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                MapTile { x: 100, y: 100, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, count: None, contents: vec![] }, MapItem { id: 300, count: None, contents: vec![] }] },
                MapTile { x: 100, y: 100, z: 8, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 7 }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        assert_eq!(sm.floor_change_at(100, 100, 7), FloorChange::DOWN);
        assert_eq!(
            sm.resolve_floor_change(Position::new(100, 100, 7)),
            Some(Position::new(100, 100, 8))
        );
        assert_eq!(sm.resolve_floor_change(Position::new(100, 100, 8)), None);
    }

    #[test]
    fn triggers_up_needs_height_three() {
        use formats::items_xml::FloorChange;
        let h = |sid| ItemType { group: 5, flags: 1 << 3, server_id: sid, client_id: sid, always_on_top: false, top_order: 0, has_height: true, floor_change: FloorChange::NONE };
        let items = ItemsOtb {
            major_version: 3, minor_version: 57, build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 1, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
                h(301), h(302), h(303),
            ],
        };
        let tile = |x, ids: Vec<u16>| MapTile { x, y: 100, z: 7, flags: 0, house_id: None,
            items: ids.into_iter().map(|id| MapItem { id, count: None, contents: vec![] }).collect() };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                tile(100, vec![100, 301, 302]),       // height 2 -> no
                tile(101, vec![100, 301, 302, 303]),  // height 3 -> yes
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 7 }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        assert!(!sm.triggers_up(Position::new(100, 100, 7)), "height 2 does not trigger");
        assert!(sm.triggers_up(Position::new(101, 100, 7)), "height 3 triggers");
    }
}
