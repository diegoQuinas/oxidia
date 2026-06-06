//! Immutable world map for M3: a ground-tile lookup + a spawn point.
//! Ground client ids are resolved once from items.otb (server_id -> client_id).

use std::collections::HashMap;

use formats::otb::ItemsOtb;
use formats::otbm::OtbmMap;
use protocol::map_description::GroundSource;

use crate::Position;

/// Default spawn if the map has no towns (mid-map, ground level).
const FALLBACK_SPAWN: Position = Position::new(1000, 1000, 7);

pub struct StaticMap {
    ground: HashMap<(u16, u16, u8), u16>,
    spawn: Position,
}

impl StaticMap {
    /// Build from a parsed map + item dictionary. The ground client id of a tile
    /// is its first item's id mapped through items.otb (server_id -> client_id).
    pub fn from_formats(map: &OtbmMap, items: &ItemsOtb) -> Self {
        let server_to_client: HashMap<u16, u16> =
            items.items.iter().map(|it| (it.server_id, it.client_id)).collect();

        let mut ground = HashMap::new();
        for tile in &map.tiles {
            let Some(first) = tile.items.first() else { continue };
            let Some(&client_id) = server_to_client.get(&first.id) else { continue };
            ground.insert((tile.x, tile.y, tile.z), client_id);
        }

        let spawn = map
            .towns
            .first()
            .map(|t| Position::new(t.x, t.y, t.z))
            .unwrap_or(FALLBACK_SPAWN);

        Self { ground, spawn }
    }

    pub fn spawn(&self) -> Position {
        self.spawn
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
}
