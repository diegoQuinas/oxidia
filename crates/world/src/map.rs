//! Immutable world map: a per-tile item stack + a spawn point.
//! Client ids are resolved once from items.otb (server_id -> client_id).

use std::collections::{HashMap, HashSet};

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

/// Wire-ordered items for one tile, split around the creature slot.
struct TileStack {
    /// `[ground, ...top items (by top_order), ...down items]`, capped at 10.
    items: Vec<WireItem>,
    /// `items[..pre_creature_len]` render below a creature (ground + top items).
    pre_creature_len: usize,
}

/// Resolve an `items.otb` entry into its wire form, mirroring TFS
/// `NetworkMessage::addItem`: stackable items carry a count byte, splash/fluid a
/// fluid-type byte, animated items a phase byte. Static map items have no OTBM
/// subtype parsed yet, so count/fluid default to 1/0 (byte-correct on the wire).
fn wire_item(it: &formats::otb::ItemType) -> WireItem {
    let subtype = if it.is_stackable() {
        Some(1)
    } else if it.is_fluid_or_splash() {
        Some(0)
    } else {
        None
    };
    WireItem { client_id: it.client_id, subtype, animated: it.is_animated() }
}

pub struct StaticMap {
    tiles: HashMap<(u16, u16, u8), TileStack>,
    blocked: HashSet<(u16, u16, u8)>,
    spawn: Position,
}

impl StaticMap {
    /// Build from a parsed map + item dictionary. Each tile becomes a wire-ordered
    /// stack: ground (`items[0]`), then always-on-top items sorted by `top_order`,
    /// then the remaining "down" items, capped at 10 things (TFS stackpos cap).
    pub fn from_formats(map: &OtbmMap, items: &ItemsOtb) -> Self {
        let by_id: HashMap<u16, &formats::otb::ItemType> =
            items.items.iter().map(|it| (it.server_id, it)).collect();

        let mut tiles = HashMap::new();
        let mut blocked = HashSet::new();
        for tile in &map.tiles {
            let mut ground: Option<WireItem> = None;
            let mut top: Vec<(u8, WireItem)> = Vec::new(); // (top_order, item)
            let mut down: Vec<WireItem> = Vec::new();
            for (i, mi) in tile.items.iter().enumerate() {
                let Some(it) = by_id.get(&mi.id) else { continue };
                let wi = wire_item(it);
                if i == 0 {
                    ground = Some(wi);
                } else if it.always_on_top {
                    top.push((it.top_order, wi));
                } else {
                    down.push(wi);
                }
            }

            if let Some(ground_item) = ground {
                top.sort_by_key(|(order, _)| *order); // stable: file order on ties
                let mut stack: Vec<WireItem> = Vec::with_capacity(1 + top.len() + down.len());
                stack.push(ground_item);
                stack.extend(top.iter().map(|(_, wi)| *wi));
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

    pub fn spawn(&self) -> Position {
        self.spawn
    }

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
            items: vec![ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0 }],
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
                items: vec![MapItem { id: 100, contents: vec![] }],
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
    fn walkability_uses_block_solid_flag() {
        // server 100 = walkable ground; server 200 = block-solid wall on the same tile.
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0 },
                ItemType { group: 5, flags: 0x0000_0001, server_id: 200, client_id: 1059, always_on_top: false, top_order: 0 },
            ],
        };
        let map = OtbmMap {
            width: 100, height: 100, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                // plain ground -> walkable
                MapTile { x: 95, y: 117, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, contents: vec![] }] },
                // ground + block-solid wall -> not walkable
                MapTile { x: 96, y: 117, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, contents: vec![] }, MapItem { id: 200, contents: vec![] }] },
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
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0 },
                ItemType { group: 5, flags: 1 << 13, server_id: 200, client_id: 1000, always_on_top: true, top_order: 2 },
                ItemType { group: 5, flags: 1 << 13, server_id: 201, client_id: 1001, always_on_top: true, top_order: 1 },
                ItemType { group: 5, flags: 0, server_id: 300, client_id: 2000, always_on_top: false, top_order: 0 },
            ],
        };
        let map = OtbmMap {
            width: 100, height: 100, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![MapTile {
                x: 95, y: 117, z: 7, flags: 0, house_id: None,
                items: vec![
                    MapItem { id: 100, contents: vec![] },
                    MapItem { id: 200, contents: vec![] },
                    MapItem { id: 201, contents: vec![] },
                    MapItem { id: 300, contents: vec![] },
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
        }];
        let mut tile_items = vec![MapItem { id: 1, contents: vec![] }];
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
        }];
        let mut tile_items = vec![MapItem { id: 1, contents: vec![] }];
        for sid in 2..=12u16 {
            item_defs.push(ItemType {
                group: 5, flags: 1 << 13, server_id: sid, client_id: 6000 + sid,
                always_on_top: true, top_order: 0,
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
        assert_eq!(slices.pre_creature.len(), 10); // ground + 9 top items
        assert_eq!(cids(slices.pre_creature), vec![5000, 6002, 6003, 6004, 6005, 6006, 6007, 6008, 6009, 6010]);
        assert!(slices.post_creature.is_empty());
        assert_eq!(sm.creature_stackpos(95, 117, 7), 10);
    }
}
