//! Immutable world map: a per-tile item stack + a spawn point.
//! Client ids are resolved once from items.otb (server_id -> client_id).

use std::collections::{HashMap, HashSet, VecDeque};

use formats::items_xml::{FloorChange, ItemsXml};
use formats::otb::ItemsOtb;
use formats::otbm::OtbmMap;
use protocol::map_description::{TileSlices, TileSource, WireItem};

use crate::pathfinding::{self, FindPathParams};
use crate::{Direction, Position};

/// Default spawn if the map has no towns (mid-map, ground level).
const FALLBACK_SPAWN: Position = Position::new(1000, 1000, 7);

/// `items.otb` `FLAG_BLOCK_SOLID` (bit 0 of the per-item flags word).
const FLAG_BLOCK_SOLID: u32 = 1 << 0;

/// Maximum things (items + creature) the client renders per tile.
const MAX_TILE_THINGS: usize = 10;

/// Which equipment slot(s) an item may occupy, derived from items.xml
/// slotType/weaponType. Slot numbers are TFS `CONST_SLOT_*` (head=1 … ammo=10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EquipSlot {
    Head,     // 1
    Necklace, // 2
    Backpack, // 3
    Armor,    // 4  (slotType "body")
    Hand,     // 5 (right) or 6 (left): weapons, shields, two-handed
    Legs,     // 7
    Feet,     // 8
    Ring,     // 9
    Ammo,     // 10
}

impl EquipSlot {
    /// Map an items.xml `slotType` / `weaponType` pair to an equip slot.
    /// `None` → the item is not equippable.
    pub fn from_xml(slot_type: &str, weapon_type: &str) -> Option<Self> {
        match slot_type {
            "head" => Some(Self::Head),
            "necklace" => Some(Self::Necklace),
            "backpack" => Some(Self::Backpack),
            "body" => Some(Self::Armor),
            "legs" => Some(Self::Legs),
            "feet" => Some(Self::Feet),
            "ring" => Some(Self::Ring),
            "ammo" => Some(Self::Ammo),
            "two-handed" => Some(Self::Hand),
            _ => match weapon_type {
                "sword" | "axe" | "club" | "distance" | "wand" | "shield" => Some(Self::Hand),
                "ammunition" => Some(Self::Ammo),
                _ => None,
            },
        }
    }

    /// True if this item may be placed in the given 1-based inventory slot.
    pub fn admits(self, slot: u8) -> bool {
        match self {
            Self::Head => slot == 1,
            Self::Necklace => slot == 2,
            Self::Backpack => slot == 3,
            Self::Armor => slot == 4,
            Self::Hand => slot == 5 || slot == 6,
            Self::Legs => slot == 7,
            Self::Feet => slot == 8,
            Self::Ring => slot == 9,
            Self::Ammo => slot == 10,
        }
    }
}

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
    /// Client sprite id, for building wire items when an item is moved.
    pub client_id: u16,
    /// Whether the item carries a `0xFE` animation-phase byte on the wire.
    pub animated: bool,
    /// Whether the item may be picked up / moved by a player.
    pub moveable: bool,
    /// Which equipment slot this item admits, or `None` if not equippable.
    pub equip_slot: Option<EquipSlot>,
    /// True if this item is a container/bag (`ITEM_GROUP_CONTAINER`).
    pub is_container: bool,
    /// Maximum items the container holds (0 for non-containers, 8+ for bags).
    pub container_capacity: u8,
}

impl ItemMeta {
    /// Plural form for stackable count > 1, mirroring TFS `ItemType::getPluralName`
    /// (`items.h`): the explicit `plural` attribute when set, else `name + "s"`
    /// when `show_count`, else the bare `name`. This is why `crystal coin` (no
    /// `plural` in items.xml) still reads "crystal coins".
    pub fn plural_name(&self) -> String {
        if !self.plural.is_empty() {
            self.plural.clone()
        } else if self.show_count {
            format!("{}s", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// Wire-ordered items for one tile, split around the creature slot.
#[derive(Clone)]
pub(crate) struct TileStack {
    /// `[ground, ...top items (by top_order), ...down items]`, capped at 10.
    pub(crate) items: Vec<WireItem>,
    /// Server ids parallel to `items` (same order/length) for look-at metadata.
    pub(crate) server_ids: Vec<u16>,
    /// OTBM stack counts parallel to `items` (None when unspecified). Drives the
    /// look-at text ("You see 50 gold coins.") for stackable items.
    pub(crate) counts: Vec<Option<u8>>,
    /// `items[..pre_creature_len]` render below a creature (ground + top items).
    pub(crate) pre_creature_len: usize,
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
    WireItem {
        client_id: it.client_id,
        subtype,
        animated: it.is_animated(),
    }
}

/// OTBM tile flag `PROTECTIONZONE` — bit 0 of `MapTile.flags` (iomap.h:60).
/// TFS maps this to `TILESTATE_PROTECTIONZONE` (tile.h:33) at runtime; we keep
/// the OTBM flag name here since we read it from the parsed `MapTile.flags`.
const OTBM_TILEFLAG_PROTECTIONZONE: u32 = 1 << 0;

pub struct StaticMap {
    tiles: HashMap<(u16, u16, u8), TileStack>,
    blocked: HashSet<(u16, u16, u8)>,
    /// Tiles whose stack contains a block-projectile item — opaque to line of
    /// sight (TFS `isTileClear`). Built at load alongside `blocked`.
    block_projectile: HashSet<(u16, u16, u8)>,
    /// Per-tile floor-change flags (union of the tile's items), for stairs.
    floor_change: HashMap<(u16, u16, u8), FloorChange>,
    /// Per-tile cumulative item height (count of `has_height` items).
    tile_height: HashMap<(u16, u16, u8), u8>,
    /// Tiles whose OTBM `flags & OTBM_TILEFLAG_PROTECTIONZONE != 0` — precomputed
    /// at load, mirroring the `blocked` and `floor_change` precompute pattern.
    protection_zone: HashSet<(u16, u16, u8)>,
    spawn: Position,
    /// Towns and their temple positions, retained for GM `/temple` lookups.
    towns: Vec<formats::otbm::Town>,
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
        let mut block_projectile: HashSet<(u16, u16, u8)> = HashSet::new();
        let mut floor_change = HashMap::new();
        let mut tile_height = HashMap::new();
        let mut protection_zone = HashSet::new();
        for tile in &map.tiles {
            let mut ground: Option<(WireItem, u16, Option<u8>)> = None;
            let mut top: Vec<(u8, WireItem, u16, Option<u8>)> = Vec::new(); // (top_order, item, sid, count)
            let mut down: Vec<(WireItem, u16, Option<u8>)> = Vec::new();
            for (i, mi) in tile.items.iter().enumerate() {
                let Some(it) = by_id.get(&mi.id) else {
                    continue;
                };
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
                for (_, wi, sid, c) in &top {
                    items.push(*wi);
                    server_ids.push(*sid);
                    counts.push(*c);
                }
                let pre_creature_len = items.len().min(MAX_TILE_THINGS);
                for (wi, sid, c) in &down {
                    items.push(*wi);
                    server_ids.push(*sid);
                    counts.push(*c);
                }
                items.truncate(MAX_TILE_THINGS);
                server_ids.truncate(MAX_TILE_THINGS);
                counts.truncate(MAX_TILE_THINGS);
                tiles.insert(
                    (tile.x, tile.y, tile.z),
                    TileStack {
                        items,
                        server_ids,
                        counts,
                        pre_creature_len,
                    },
                );
            }

            let solid = tile.items.iter().any(|mi| {
                by_id
                    .get(&mi.id)
                    .is_some_and(|it| it.flags & FLAG_BLOCK_SOLID != 0)
            });
            if solid {
                blocked.insert((tile.x, tile.y, tile.z));
            }

            // Vertical metadata: union of floor-change flags + summed item heights.
            // Also precompute block-projectile for line-of-sight checks.
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
                    if it.is_block_projectile() {
                        block_projectile.insert((tile.x, tile.y, tile.z));
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

        Self {
            tiles,
            blocked,
            block_projectile,
            floor_change,
            tile_height,
            protection_zone,
            spawn,
            towns: map.towns.clone(),
            item_meta: HashMap::new(),
        }
    }

    /// Populate the look-at metadata catalog from items.otb (flags) + items.xml
    /// (name/description/weight). Call once at boot, after construction. Tests
    /// that exercise look-at call this explicitly with a small fixture.
    pub fn load_item_metadata(&mut self, otb: &ItemsOtb, xml: &ItemsXml) {
        for it in &otb.items {
            let x = xml.attrs(it.server_id);
            self.item_meta.insert(
                it.server_id,
                ItemMeta {
                    name: x.map(|a| a.name.clone()).unwrap_or_default(),
                    article: x.map(|a| a.article.clone()).unwrap_or_default(),
                    plural: x.map(|a| a.plural.clone()).unwrap_or_default(),
                    description: x.map(|a| a.description.clone()).unwrap_or_default(),
                    weight: x.map(|a| a.weight).unwrap_or(0),
                    show_count: x.map(|a| a.show_count).unwrap_or(true),
                    stackable: it.is_stackable(),
                    pickupable: it.is_pickupable(),
                    client_id: it.client_id,
                    animated: it.is_animated(),
                    moveable: it.is_moveable(),
                    equip_slot: x.and_then(|a| EquipSlot::from_xml(&a.slot_type, &a.weapon_type)),
                    is_container: it.is_container(),
                    container_capacity: x.map(|a| a.container_size).unwrap_or(0),
                },
            );
        }
    }

    /// Look-at metadata for `server_id`, or `None` if not catalogued.
    pub fn item_meta(&self, server_id: u16) -> Option<&ItemMeta> {
        self.item_meta.get(&server_id)
    }

    /// Temple position of the town with the given case-insensitive name, if any.
    pub fn town_temple_by_name(&self, name: &str) -> Option<Position> {
        self.towns
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .map(|t| Position::new(t.x, t.y, t.z))
    }

    /// Temple position of the town with the given id, if any.
    pub fn town_temple_by_id(&self, id: u32) -> Option<Position> {
        self.towns
            .iter()
            .find(|t| t.id == id)
            .map(|t| Position::new(t.x, t.y, t.z))
    }

    /// Find an item's server id by case-insensitive name (singular or plural).
    /// Returns the lowest matching server id when several items share a name.
    /// Linear scan — fine for the GM `/item` command, which is not a hot path.
    pub fn find_item_id_by_name(&self, name: &str) -> Option<u16> {
        self.item_meta
            .iter()
            .filter(|(_, m)| {
                m.name.eq_ignore_ascii_case(name) || m.plural_name().eq_ignore_ascii_case(name)
            })
            .map(|(&sid, _)| sid)
            .min()
    }

    /// How many of a tile's items render below a creature (ground + top items).
    pub fn tile_pre_creature_len(&self, pos: Position) -> usize {
        self.tiles
            .get(&(pos.x, pos.y, pos.z))
            .map_or(0, |st| st.pre_creature_len)
    }

    /// The server id of the item at index `idx` in a tile's stack, or `None`.
    pub fn tile_item_server_id(&self, pos: Position, idx: usize) -> Option<u16> {
        self.tiles
            .get(&(pos.x, pos.y, pos.z))
            .and_then(|st| st.server_ids.get(idx).copied())
    }

    /// The OTBM stack count of the item at index `idx` (None if unspecified).
    pub fn tile_item_count(&self, pos: Position, idx: usize) -> Option<u8> {
        self.tiles
            .get(&(pos.x, pos.y, pos.z))
            .and_then(|st| st.counts.get(idx).copied().flatten())
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
        self.tile_height
            .get(&(pos.x, pos.y, pos.z))
            .copied()
            .unwrap_or(0)
    }

    /// Is there a block-projectile item on this tile? Used for line-of-sight
    /// checks (TFS `isTileClear`). Populated at load alongside `blocked`.
    pub fn is_block_projectile(&self, pos: Position) -> bool {
        self.block_projectile.contains(&(pos.x, pos.y, pos.z))
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
            if self
                .floor_change_at(dx, dy - 1, dz)
                .contains(FloorChange::SOUTH_ALT)
            {
                dy -= 2;
            } else if self
                .floor_change_at(dx - 1, dy, dz)
                .contains(FloorChange::EAST_ALT)
            {
                dx -= 2;
            } else {
                let down = self.floor_change_at(dx, dy, dz);
                if down.contains(FloorChange::NORTH) {
                    dy += 1;
                }
                if down.contains(FloorChange::SOUTH) {
                    dy -= 1;
                }
                if down.contains(FloorChange::SOUTH_ALT) {
                    dy -= 2;
                }
                if down.contains(FloorChange::EAST) {
                    dx -= 1;
                }
                if down.contains(FloorChange::EAST_ALT) {
                    dx -= 2;
                }
                if down.contains(FloorChange::WEST) {
                    dx += 1;
                }
            }
            return to_position(dx, dy, dz);
        }

        // Plain directional (no DOWN) -> ascend one floor.
        let dz = i32::from(from.z) - 1;
        if fc.contains(FloorChange::NORTH) {
            dy -= 1;
        }
        if fc.contains(FloorChange::SOUTH) {
            dy += 1;
        }
        if fc.contains(FloorChange::EAST) {
            dx += 1;
        }
        if fc.contains(FloorChange::WEST) {
            dx -= 1;
        }
        if fc.contains(FloorChange::SOUTH_ALT) {
            dy += 2;
        }
        if fc.contains(FloorChange::EAST_ALT) {
            dx += 2;
        }
        to_position(dx, dy, dz)
    }

    // -------------------------------------------------------------------------
    // Line-of-sight + throw-range (faithful port of TFS map.cpp:486-624)
    // -------------------------------------------------------------------------

    /// TFS `isTileClear`: a tile is opaque to sight if it holds a block-projectile
    /// item, or (when `block_floor`) if it has ground at all. (`map.cpp` helper.)
    fn is_tile_clear(&self, x: u16, y: u16, z: u8, block_floor: bool) -> bool {
        let key = (x, y, z);
        if self.block_projectile.contains(&key) {
            return false;
        }
        if block_floor && self.tiles.contains_key(&key) {
            return false;
        }
        true
    }

    /// TFS anonymous `checkSlightLine`: walk along x, sampling y by slope.
    fn check_slight_line(&self, x0: u16, y0: u16, x1: u16, y1: u16, z: u8) -> bool {
        let dx = f32::from(x1) - f32::from(x0);
        let slope = if dx == 0.0 {
            1.0
        } else {
            (f32::from(y1) - f32::from(y0)) / dx
        };
        let mut yi = f32::from(y0) + slope;
        let mut x = x0 + 1;
        while x < x1 {
            // 0.1 guard mirrors TFS: "necessary to avoid loss of precision during calculation"
            // Coords are bounded by map dimensions; cast is safe (mirrors TFS float math).
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            if !self.is_tile_clear(x, (yi + 0.1).floor() as u16, z, false) {
                return false;
            }
            yi += slope;
            x += 1;
        }
        true
    }

    /// TFS anonymous `checkSteepLine`: walk along y, sampling x by slope (args
    /// pre-swapped by `check_sight_line`).
    fn check_steep_line(&self, x0: u16, y0: u16, x1: u16, y1: u16, z: u8) -> bool {
        let dx = f32::from(x1) - f32::from(x0);
        let slope = if dx == 0.0 {
            1.0
        } else {
            (f32::from(y1) - f32::from(y0)) / dx
        };
        let mut yi = f32::from(y0) + slope;
        let mut x = x0 + 1;
        while x < x1 {
            // 0.1 guard mirrors TFS; coords bounded by map dimensions (faithful cast).
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            if !self.is_tile_clear((yi + 0.1).floor() as u16, x, z, false) {
                return false;
            }
            yi += slope;
            x += 1;
        }
        true
    }

    /// TFS `Map::checkSightLine` — pick the steep/slight walk by dominant axis.
    /// When steep (|dy| > |dx|), x and y are swapped before calling `check_steep_line`,
    /// exactly as TFS does (`checkSteepLine(y0, x0, y1, x1, z)`).
    fn check_sight_line(&self, x0: u16, y0: u16, x1: u16, y1: u16, z: u8) -> bool {
        if x0 == x1 && y0 == y1 {
            return true;
        }
        let dy = i32::from(y1) - i32::from(y0);
        let dx = i32::from(x1) - i32::from(x0);
        if dy.abs() > dx.abs() {
            if y1 > y0 {
                return self.check_steep_line(y0, x0, y1, x1, z);
            }
            return self.check_steep_line(y1, x1, y0, x0, z);
        }
        if x0 > x1 {
            return self.check_slight_line(x1, y1, x0, y0, z);
        }
        self.check_slight_line(x0, y0, x1, y1, z)
    }

    /// TFS `Map::isSightClear` (faithful). Same-floor fast path + the multifloor
    /// branches. `same_floor=false` matches the throw default.
    pub fn is_sight_clear(&self, from: Position, to: Position, same_floor: bool) -> bool {
        if from.z == to.z {
            let ddx = (i32::from(from.x) - i32::from(to.x)).abs();
            let ddy = (i32::from(from.y) - i32::from(to.y)).abs();
            if ddx < 2 && ddy < 2 {
                return true;
            }
            let clear = self.check_sight_line(from.x, from.y, to.x, to.y, from.z);
            if clear || same_floor {
                return clear;
            }
            if from.z == 0 {
                return true;
            }
            let nz = from.z - 1;
            return self.is_tile_clear(from.x, from.y, nz, true)
                && self.is_tile_clear(to.x, to.y, nz, true)
                && self.check_sight_line(from.x, from.y, to.x, to.y, nz);
        }
        if same_floor {
            return false;
        }
        if (from.z < 8 && to.z > 7) || (from.z > 7 && to.z < 8) {
            return false;
        }
        if from.z > to.z {
            if (i32::from(from.z) - i32::from(to.z)).abs() > 1 {
                return false;
            }
            let nz = from.z - 1;
            return self.is_tile_clear(from.x, from.y, nz, true)
                && self.check_sight_line(from.x, from.y, to.x, to.y, nz);
        }
        let mut z = from.z;
        while z < to.z {
            if !self.is_tile_clear(to.x, to.y, z, true) {
                return false;
            }
            z += 1;
        }
        self.check_sight_line(from.x, from.y, to.x, to.y, from.z)
    }

    /// TFS `Map::canThrowObjectTo`. Range default = client viewport (8×6); LOS on.
    pub fn can_throw_object_to(&self, from: Position, to: Position) -> bool {
        const RANGE_X: i32 = 8; // Map::maxClientViewportX (map.h:162)
        const RANGE_Y: i32 = 6; // Map::maxClientViewportY (map.h:163)
        if (i32::from(from.x) - i32::from(to.x)).abs() > RANGE_X
            || (i32::from(from.y) - i32::from(to.y)).abs() > RANGE_Y
        {
            return false;
        }
        self.is_sight_clear(from, to, false)
    }
}

impl StaticMap {
    /// Bounds-check a world coordinate down to a `(u16, u16, u8)` tile key.
    pub(crate) fn key(x: i32, y: i32, z: i32) -> Option<(u16, u16, u8)> {
        if !(0..=i32::from(u16::MAX)).contains(&x)
            || !(0..=i32::from(u16::MAX)).contains(&y)
            || !(0..=i32::from(u8::MAX)).contains(&z)
        {
            return None;
        }
        Some((x as u16, y as u16, z as u8))
    }

    /// Snapshot a tile's full stack for the copy-on-write overlay. `None` if the
    /// tile has no entry (no ground).
    #[allow(dead_code)] // used by materialize (M10.1 Task 5)
    pub(crate) fn tile_stack_clone(&self, pos: Position) -> Option<TileStack> {
        self.tiles.get(&(pos.x, pos.y, pos.z)).cloned()
    }

    /// Check whether a creature can walk onto `pos`, given other creature positions.
    /// `self_id` is excluded from the creature occupancy check.
    pub fn can_creature_walk_to(
        &self,
        self_id: u32,
        pos: Position,
        creatures: &[(u16, u16, u32)],
    ) -> bool {
        if !self.is_walkable(pos) {
            return false;
        }
        // Check if another creature occupies this tile (excluding self).
        let occupied = creatures
            .iter()
            .any(|&(cx, cy, cid)| cid != self_id && cx == pos.x && cy == pos.y);
        !occupied
    }

    /// Run A* pathfinding from `start` to a tile satisfying `condition`.
    /// Creature positions are passed for occupancy penalties in the search.
    pub fn get_path_matching(
        &self,
        start: Position,
        target: Position,
        creatures: &[Position],
        params: &FindPathParams,
        condition: pathfinding::FrozenPathingConditionCall,
    ) -> VecDeque<Direction> {
        let creature_coords: Vec<(u16, u16)> = creatures.iter().map(|p| (p.x, p.y)).collect();
        let is_walkable = |x: u16, y: u16| self.is_walkable(Position::new(x, y, start.z));
        pathfinding::get_path_matching(
            start,
            target,
            &creature_coords,
            params,
            &condition,
            &is_walkable,
        )
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

/// A read view over the immutable `StaticMap` plus the actor's runtime overlay.
/// Tile reads check the overlay first (a materialised dynamic stack), then fall
/// back to the static map. Passed to the map encoder in place of `&StaticMap`.
pub(crate) struct MergedTiles<'a> {
    pub(crate) base: &'a StaticMap,
    pub(crate) dynamic: &'a std::collections::HashMap<(u16, u16, u8), TileStack>,
}

impl TileSource for MergedTiles<'_> {
    fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>> {
        let key = StaticMap::key(x, y, z)?;
        if let Some(st) = self.dynamic.get(&key) {
            return Some(TileSlices {
                pre_creature: &st.items[..st.pre_creature_len],
                post_creature: &st.items[st.pre_creature_len..],
            });
        }
        self.base.tile(x, y, z)
    }

    fn creature_stackpos(&self, x: i32, y: i32, z: i32) -> u8 {
        if let Some(key) = StaticMap::key(x, y, z) {
            if let Some(st) = self.dynamic.get(&key) {
                return st.pre_creature_len as u8;
            }
        }
        self.base.creature_stackpos(x, y, z)
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
            items: vec![ItemType {
                group: 1,
                flags: 0,
                server_id: 100,
                client_id: 4526,
                always_on_top: false,
                top_order: 0,
                has_height: false,
                floor_change: formats::items_xml::FloorChange::NONE,
            }],
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
                items: vec![MapItem {
                    id: 100,
                    count: None,
                    contents: vec![],
                }],
            }],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 95,
                y: 117,
                z: 7,
            }],
            waypoints: vec![],
        };
        (map, items)
    }

    #[test]
    fn find_item_id_by_name_matches_name_and_derived_plural() {
        let (map, items) = tiny_map();
        let mut sm = StaticMap::from_formats(&map, &items);
        sm.item_meta.insert(
            2160,
            ItemMeta {
                name: "crystal coin".into(),
                show_count: true,
                stackable: true,
                client_id: 1,
                ..Default::default()
            },
        );
        sm.item_meta.insert(
            2152,
            ItemMeta {
                name: "platinum coin".into(),
                client_id: 2,
                ..Default::default()
            },
        );
        assert_eq!(sm.find_item_id_by_name("crystal coin"), Some(2160));
        assert_eq!(sm.find_item_id_by_name("CRYSTAL COIN"), Some(2160)); // case-insensitive
        assert_eq!(sm.find_item_id_by_name("crystal coins"), Some(2160)); // derived plural
        assert_eq!(sm.find_item_id_by_name("platinum coin"), Some(2152));
        assert_eq!(sm.find_item_id_by_name("nonexistent"), None);
    }

    #[test]
    fn find_item_id_by_name_breaks_ties_on_lowest_id() {
        let (map, items) = tiny_map();
        let mut sm = StaticMap::from_formats(&map, &items);
        sm.item_meta.insert(
            500,
            ItemMeta {
                name: "door".into(),
                ..Default::default()
            },
        );
        sm.item_meta.insert(
            400,
            ItemMeta {
                name: "door".into(),
                ..Default::default()
            },
        );
        assert_eq!(sm.find_item_id_by_name("door"), Some(400));
    }

    #[test]
    fn town_temple_lookup_by_name_and_id() {
        let (map, items) = tiny_map(); // town Thais id 1 at (95, 117, 7)
        let sm = StaticMap::from_formats(&map, &items);
        assert_eq!(
            sm.town_temple_by_name("Thais"),
            Some(Position::new(95, 117, 7))
        );
        assert_eq!(
            sm.town_temple_by_name("thais"),
            Some(Position::new(95, 117, 7))
        ); // case-insensitive
        assert_eq!(sm.town_temple_by_id(1), Some(Position::new(95, 117, 7)));
        assert_eq!(sm.town_temple_by_name("Venore"), None);
        assert_eq!(sm.town_temple_by_id(99), None);
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
            Town {
                id: 1,
                name: "Venore".into(),
                x: 95,
                y: 117,
                z: 7,
            },
            Town {
                id: 5,
                name: "Ab'Dendriel".into(),
                x: 200,
                y: 300,
                z: 7,
            },
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
                ItemType {
                    group: 1,
                    flags: 0,
                    server_id: 100,
                    client_id: 4526,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: formats::items_xml::FloorChange::NONE,
                },
                ItemType {
                    group: 5,
                    flags: 0x0000_0001,
                    server_id: 200,
                    client_id: 1059,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: formats::items_xml::FloorChange::NONE,
                },
            ],
        };
        let map = OtbmMap {
            width: 100,
            height: 100,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![
                // plain ground -> walkable
                MapTile {
                    x: 95,
                    y: 117,
                    z: 7,
                    flags: 0,
                    house_id: None,
                    items: vec![MapItem {
                        id: 100,
                        count: None,
                        contents: vec![],
                    }],
                },
                // ground + block-solid wall -> not walkable
                MapTile {
                    x: 96,
                    y: 117,
                    z: 7,
                    flags: 0,
                    house_id: None,
                    items: vec![
                        MapItem {
                            id: 100,
                            count: None,
                            contents: vec![],
                        },
                        MapItem {
                            id: 200,
                            count: None,
                            contents: vec![],
                        },
                    ],
                },
            ],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 95,
                y: 117,
                z: 7,
            }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        assert!(
            sm.is_walkable(Position::new(95, 117, 7)),
            "plain ground walkable"
        );
        assert!(
            !sm.is_walkable(Position::new(96, 117, 7)),
            "block-solid wall not walkable"
        );
        assert!(
            !sm.is_walkable(Position::new(1, 1, 7)),
            "no ground not walkable"
        );
    }

    #[test]
    fn builds_ordered_stack_with_pre_creature_split() {
        use protocol::map_description::TileSource;
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType {
                    group: 1,
                    flags: 0,
                    server_id: 100,
                    client_id: 4526,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: formats::items_xml::FloorChange::NONE,
                },
                ItemType {
                    group: 5,
                    flags: 1 << 13,
                    server_id: 200,
                    client_id: 1000,
                    always_on_top: true,
                    top_order: 2,
                    has_height: false,
                    floor_change: formats::items_xml::FloorChange::NONE,
                },
                ItemType {
                    group: 5,
                    flags: 1 << 13,
                    server_id: 201,
                    client_id: 1001,
                    always_on_top: true,
                    top_order: 1,
                    has_height: false,
                    floor_change: formats::items_xml::FloorChange::NONE,
                },
                ItemType {
                    group: 5,
                    flags: 0,
                    server_id: 300,
                    client_id: 2000,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: formats::items_xml::FloorChange::NONE,
                },
            ],
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
                items: vec![
                    MapItem {
                        id: 100,
                        count: None,
                        contents: vec![],
                    },
                    MapItem {
                        id: 200,
                        count: None,
                        contents: vec![],
                    },
                    MapItem {
                        id: 201,
                        count: None,
                        contents: vec![],
                    },
                    MapItem {
                        id: 300,
                        count: None,
                        contents: vec![],
                    },
                ],
            }],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 95,
                y: 117,
                z: 7,
            }],
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
            group: 1,
            flags: 0,
            server_id: 1,
            client_id: 5000,
            always_on_top: false,
            top_order: 0,
            has_height: false,
            floor_change: formats::items_xml::FloorChange::NONE,
        }];
        let mut tile_items = vec![MapItem {
            id: 1,
            count: None,
            contents: vec![],
        }];
        for sid in 2..=12u16 {
            item_defs.push(ItemType {
                group: 5,
                flags: 0,
                server_id: sid,
                client_id: 6000 + sid,
                always_on_top: false,
                top_order: 0,
                has_height: false,
                floor_change: formats::items_xml::FloorChange::NONE,
            });
            tile_items.push(MapItem {
                id: sid,
                count: None,
                contents: vec![],
            });
        }
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: item_defs,
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
                items: tile_items,
            }],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 95,
                y: 117,
                z: 7,
            }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        let slices = sm.tile(95, 117, 7).expect("tile present");
        // Ground stays in pre_creature; the first 9 down items survive (10 total).
        assert_eq!(cids(slices.pre_creature), vec![5000]);
        assert_eq!(slices.post_creature.len(), 9);
        assert_eq!(
            cids(slices.post_creature),
            vec![6002, 6003, 6004, 6005, 6006, 6007, 6008, 6009, 6010]
        );
        assert_eq!(sm.creature_stackpos(95, 117, 7), 1);
    }

    #[test]
    fn more_than_ten_top_items_cap_pre_creature_at_ten() {
        use protocol::map_description::TileSource;
        let mut item_defs = vec![ItemType {
            group: 1,
            flags: 0,
            server_id: 1,
            client_id: 5000,
            always_on_top: false,
            top_order: 0,
            has_height: false,
            floor_change: formats::items_xml::FloorChange::NONE,
        }];
        let mut tile_items = vec![MapItem {
            id: 1,
            count: None,
            contents: vec![],
        }];
        for sid in 2..=12u16 {
            item_defs.push(ItemType {
                group: 5,
                flags: 1 << 13,
                server_id: sid,
                client_id: 6000 + sid,
                always_on_top: true,
                top_order: 0,
                has_height: false,
                floor_change: formats::items_xml::FloorChange::NONE,
            });
            tile_items.push(MapItem {
                id: sid,
                count: None,
                contents: vec![],
            });
        }
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: item_defs,
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
                items: tile_items,
            }],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 95,
                y: 117,
                z: 7,
            }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        let slices = sm.tile(95, 117, 7).expect("tile present");
        assert_eq!(slices.pre_creature.len(), 10); // ground + 9 top items
        assert_eq!(
            cids(slices.pre_creature),
            vec![5000, 6002, 6003, 6004, 6005, 6006, 6007, 6008, 6009, 6010]
        );
        assert!(slices.post_creature.is_empty());
        assert_eq!(sm.creature_stackpos(95, 117, 7), 10);
    }

    #[test]
    fn pz_flag_detected_on_flagged_tile() {
        // OTBM tile flag PROTECTIONZONE = 1<<0 (iomap.h:60).
        // A tile with flags & 1 == 1 must be reported as PZ; one without must not.
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![ItemType {
                group: 1,
                flags: 0,
                server_id: 100,
                client_id: 1,
                always_on_top: false,
                top_order: 0,
                has_height: false,
                floor_change: formats::items_xml::FloorChange::NONE,
            }],
        };
        let map = OtbmMap {
            width: 200,
            height: 200,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![
                // PZ tile: flags = 1 (OTBM_TILEFLAG_PROTECTIONZONE)
                MapTile {
                    x: 100,
                    y: 100,
                    z: 7,
                    flags: 1,
                    house_id: None,
                    items: vec![MapItem {
                        id: 100,
                        count: None,
                        contents: vec![],
                    }],
                },
                // Non-PZ tile: flags = 0
                MapTile {
                    x: 101,
                    y: 100,
                    z: 7,
                    flags: 0,
                    house_id: None,
                    items: vec![MapItem {
                        id: 100,
                        count: None,
                        contents: vec![],
                    }],
                },
            ],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 100,
                y: 100,
                z: 7,
            }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        assert!(
            sm.is_protection_zone(Position::new(100, 100, 7)),
            "PZ tile should be PZ"
        );
        assert!(
            !sm.is_protection_zone(Position::new(101, 100, 7)),
            "non-PZ tile should not be PZ"
        );
        assert!(
            !sm.is_protection_zone(Position::new(99, 99, 7)),
            "absent tile should not be PZ"
        );
    }

    #[test]
    fn temple_for_returns_spawn() {
        let (map, items) = tiny_map();
        let sm = StaticMap::from_formats(&map, &items);
        let spawn = sm.spawn();
        // M7: everyone respawns at the single town temple (the configured spawn).
        assert_eq!(
            sm.temple_for(Position::new(200, 200, 7)),
            spawn,
            "temple_for always returns spawn in M7"
        );
    }

    #[test]
    fn floor_change_down_resolves_one_floor_below() {
        use formats::items_xml::FloorChange;
        // server 100 = ground; server 300 = a floorchange-down stair item.
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType {
                    group: 1,
                    flags: 0,
                    server_id: 100,
                    client_id: 1,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: FloorChange::NONE,
                },
                ItemType {
                    group: 5,
                    flags: 0,
                    server_id: 300,
                    client_id: 2,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: FloorChange::DOWN,
                },
            ],
        };
        let map = OtbmMap {
            width: 200,
            height: 200,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![
                MapTile {
                    x: 100,
                    y: 100,
                    z: 7,
                    flags: 0,
                    house_id: None,
                    items: vec![
                        MapItem {
                            id: 100,
                            count: None,
                            contents: vec![],
                        },
                        MapItem {
                            id: 300,
                            count: None,
                            contents: vec![],
                        },
                    ],
                },
                MapTile {
                    x: 100,
                    y: 100,
                    z: 8,
                    flags: 0,
                    house_id: None,
                    items: vec![MapItem {
                        id: 100,
                        count: None,
                        contents: vec![],
                    }],
                },
            ],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 100,
                y: 100,
                z: 7,
            }],
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
        let h = |sid| ItemType {
            group: 5,
            flags: 1 << 3,
            server_id: sid,
            client_id: sid,
            always_on_top: false,
            top_order: 0,
            has_height: true,
            floor_change: FloorChange::NONE,
        };
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType {
                    group: 1,
                    flags: 0,
                    server_id: 100,
                    client_id: 1,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: FloorChange::NONE,
                },
                h(301),
                h(302),
                h(303),
            ],
        };
        let tile = |x, ids: Vec<u16>| MapTile {
            x,
            y: 100,
            z: 7,
            flags: 0,
            house_id: None,
            items: ids
                .into_iter()
                .map(|id| MapItem {
                    id,
                    count: None,
                    contents: vec![],
                })
                .collect(),
        };
        let map = OtbmMap {
            width: 200,
            height: 200,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![
                tile(100, vec![100, 301, 302]),      // height 2 -> no
                tile(101, vec![100, 301, 302, 303]), // height 3 -> yes
            ],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 100,
                y: 100,
                z: 7,
            }],
            waypoints: vec![],
        };
        let sm = StaticMap::from_formats(&map, &items);
        assert!(
            !sm.triggers_up(Position::new(100, 100, 7)),
            "height 2 does not trigger"
        );
        assert!(
            sm.triggers_up(Position::new(101, 100, 7)),
            "height 3 triggers"
        );
    }

    // -------------------------------------------------------------------------
    // M10.1 can_throw_object_to tests
    // -------------------------------------------------------------------------

    /// Build a map for LOS / throw tests:
    ///
    /// z=7 tiles:
    ///   (100,100,7) — plain ground
    ///   (101,100,7) — ground + block-projectile wall
    ///   (102,100,7) — plain ground
    ///
    /// z=6 tiles (closing the floor-below loophole):
    ///   (100,100,6) — plain ground (makes floor-below check fail at from)
    ///   (102,100,6) — plain ground (makes floor-below check fail at to)
    ///
    /// The TFS `isSightClear` fallback checks z-1 only when the direct line is
    /// blocked AND both `from` and `to` are open on z-1. Adding ground on z-6
    /// at both endpoints prevents the fallback from bypassing the wall.
    fn throw_map() -> StaticMap {
        // server 100 = ground, server 200 = block-projectile wall
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType {
                    group: 1,
                    flags: 0,
                    server_id: 100,
                    client_id: 1,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: formats::items_xml::FloorChange::NONE,
                },
                // FLAG_BLOCK_PROJECTILE = bit 1
                ItemType {
                    group: 5,
                    flags: 1 << 1,
                    server_id: 200,
                    client_id: 2,
                    always_on_top: false,
                    top_order: 0,
                    has_height: false,
                    floor_change: formats::items_xml::FloorChange::NONE,
                },
            ],
        };
        let ground = |x: u16, y: u16, z: u8| MapTile {
            x,
            y,
            z,
            flags: 0,
            house_id: None,
            items: vec![MapItem {
                id: 100,
                count: None,
                contents: vec![],
            }],
        };
        let wall = |x: u16, y: u16| MapTile {
            x,
            y,
            z: 7,
            flags: 0,
            house_id: None,
            items: vec![
                MapItem {
                    id: 100,
                    count: None,
                    contents: vec![],
                },
                MapItem {
                    id: 200,
                    count: None,
                    contents: vec![],
                },
            ],
        };
        let map = OtbmMap {
            width: 200,
            height: 200,
            major_items: 3,
            minor_items: 57,
            description: String::new(),
            spawn_file: None,
            house_file: None,
            tiles: vec![
                ground(100, 100, 7),
                wall(101, 100), // block-projectile tile between (100,100) and (102,100)
                ground(102, 100, 7),
                // z=6 floors at both endpoints close the floor-below loophole:
                // isSightClear falls back to z-1 only when BOTH endpoints are clear
                // (no ground) on z-1; adding ground there makes is_tile_clear return
                // false (block_floor=true) so the fallback also fails → throw blocked.
                ground(100, 100, 6),
                ground(102, 100, 6),
            ],
            towns: vec![Town {
                id: 1,
                name: "Thais".into(),
                x: 100,
                y: 100,
                z: 7,
            }],
            waypoints: vec![],
        };
        StaticMap::from_formats(&map, &items)
    }

    #[test]
    fn can_throw_blocked_through_block_projectile_tile() {
        // (100,100,7)→(102,100,7): wall at (101,100,7) blocks sight, and the
        // floor-below fallback is closed by ground tiles on z=6 at both endpoints.
        let sm = throw_map();
        let from = Position::new(100, 100, 7);
        let to = Position::new(102, 100, 7);
        assert!(
            !sm.can_throw_object_to(from, to),
            "throw must be blocked by block-projectile wall with closed floor-below loophole"
        );
    }

    #[test]
    fn can_throw_true_on_clear_adjacent_line() {
        let sm = throw_map();
        let from = Position::new(100, 100, 7);
        let to = Position::new(101, 100, 7); // the wall tile itself — adjacent
        // Adjacency (ddx < 2, ddy < 2) is always clear per is_sight_clear fast-path.
        assert!(
            sm.can_throw_object_to(from, to),
            "adjacent throw must be clear (fast-path ddx<2 && ddy<2)"
        );
    }

    #[test]
    fn adjacency_is_always_clear_regardless_of_block_projectile() {
        let sm = throw_map();
        // from == wall tile itself is never useful, but from neighbour to wall:
        let from = Position::new(100, 100, 7);
        let wall = Position::new(101, 100, 7); // block-projectile tile
        // ddx=1, ddy=0 → adjacent → always clear even though the dest tile has the block
        assert!(
            sm.can_throw_object_to(from, wall),
            "adjacent (ddx<2, ddy<2) must always be sight-clear"
        );
    }

    #[test]
    fn can_throw_false_when_dx_exceeds_range() {
        let sm = throw_map();
        // dx = 9 > RANGE_X=8 → out of range → false, regardless of sight
        let from = Position::new(100, 100, 7);
        let to = Position::new(109, 100, 7); // dx = 9
        assert!(
            !sm.can_throw_object_to(from, to),
            "dx > 8 must return false (out of throw range)"
        );
    }

    // -------------------------------------------------------------------------
    // M10.2 EquipSlot tests
    // -------------------------------------------------------------------------

    #[test]
    fn equip_slot_mapping_and_admits() {
        assert_eq!(EquipSlot::from_xml("head", ""), Some(EquipSlot::Head));
        assert_eq!(EquipSlot::from_xml("body", ""), Some(EquipSlot::Armor));
        assert_eq!(EquipSlot::from_xml("", "sword"), Some(EquipSlot::Hand));
        assert_eq!(EquipSlot::from_xml("", "shield"), Some(EquipSlot::Hand));
        assert_eq!(EquipSlot::from_xml("two-handed", ""), Some(EquipSlot::Hand));
        assert_eq!(EquipSlot::from_xml("", "ammunition"), Some(EquipSlot::Ammo));
        assert_eq!(EquipSlot::from_xml("ammo", ""), Some(EquipSlot::Ammo));
        assert_eq!(EquipSlot::from_xml("", ""), None);
        assert!(EquipSlot::Head.admits(1) && !EquipSlot::Head.admits(2));
        assert!(
            EquipSlot::Hand.admits(5) && EquipSlot::Hand.admits(6) && !EquipSlot::Hand.admits(4)
        );
        assert!(EquipSlot::Ammo.admits(10) && !EquipSlot::Ammo.admits(1));
    }
}
