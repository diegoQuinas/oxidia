//! Immutable world map for M3: a ground-tile lookup + a spawn point.
//! Ground client ids are resolved once from items.otb (server_id -> client_id).

use std::collections::{HashMap, HashSet};

use formats::otb::ItemsOtb;
use formats::otbm::OtbmMap;
use protocol::map_description::GroundSource;

use crate::Position;

/// Default spawn if the map has no towns (mid-map, ground level).
const FALLBACK_SPAWN: Position = Position::new(1000, 1000, 7);

/// `items.otb` `FLAG_BLOCK_SOLID` (bit 0 of the per-item flags word).
const FLAG_BLOCK_SOLID: u32 = 1 << 0;

pub struct StaticMap {
    ground: HashMap<(u16, u16, u8), u16>,
    blocked: HashSet<(u16, u16, u8)>,
    spawn: Position,
}

impl StaticMap {
    /// Build from a parsed map + item dictionary. The ground client id of a tile
    /// is its first item's id mapped through items.otb (server_id -> client_id).
    pub fn from_formats(map: &OtbmMap, items: &ItemsOtb) -> Self {
        let server_to_client: HashMap<u16, u16> =
            items.items.iter().map(|it| (it.server_id, it.client_id)).collect();

        let server_to_flags: HashMap<u16, u32> =
            items.items.iter().map(|it| (it.server_id, it.flags)).collect();

        let mut ground = HashMap::new();
        let mut blocked = HashSet::new();
        for tile in &map.tiles {
            if let Some(first) = tile.items.first() {
                if let Some(&client_id) = server_to_client.get(&first.id) {
                    ground.insert((tile.x, tile.y, tile.z), client_id);
                }
            }
            let solid = tile.items.iter().any(|it| {
                server_to_flags.get(&it.id).is_some_and(|f| f & FLAG_BLOCK_SOLID != 0)
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

        Self { ground, blocked, spawn }
    }

    pub fn spawn(&self) -> Position {
        self.spawn
    }

    /// A tile is walkable if it has ground and no block-solid item.
    pub fn is_walkable(&self, pos: Position) -> bool {
        self.ground.contains_key(&(pos.x, pos.y, pos.z))
            && !self.blocked.contains(&(pos.x, pos.y, pos.z))
    }
}

impl GroundSource for StaticMap {
    fn ground(&self, x: i32, y: i32, z: i32) -> Option<u16> {
        if !(0..=i32::from(u16::MAX)).contains(&x)
            || !(0..=i32::from(u16::MAX)).contains(&y)
            || !(0..=i32::from(u8::MAX)).contains(&z)
        {
            return None;
        }
        self.ground.get(&(x as u16, y as u16, z as u8)).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use formats::otb::ItemType;
    use formats::otbm::{MapItem, MapTile, Town};

    fn tiny_map() -> (OtbmMap, ItemsOtb) {
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![ItemType { group: 0, flags: 0, server_id: 100, client_id: 4526 }],
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
        let (map, items) = tiny_map();
        let sm = StaticMap::from_formats(&map, &items);
        assert_eq!(sm.spawn(), Position::new(95, 117, 7));
        assert_eq!(sm.ground(95, 117, 7), Some(4526));
        assert_eq!(sm.ground(0, 0, 7), None);
        assert_eq!(sm.ground(-1, 0, 7), None);
    }

    #[test]
    fn walkability_uses_block_solid_flag() {
        // server 100 = walkable ground; server 200 = block-solid wall on the same tile.
        let items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![
                ItemType { group: 0, flags: 0, server_id: 100, client_id: 4526 },
                ItemType { group: 0, flags: 0x0000_0001, server_id: 200, client_id: 1059 },
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
}
