//! Cross-crate integration: a real `StaticMap` encodes its per-tile item stack
//! through the protocol map-description encoder, and an OTClient-faithful decoder
//! recovers the stack in wire order. Also checks `walk_update` sends the real
//! creature stackpos for a decorated tile.

use std::collections::HashMap;

use formats::otb::{ItemType, ItemsOtb};
use formats::otbm::{MapItem, MapTile, OtbmMap, Town};
use protocol::map_description::{encode, Center};
use protocol::walk;
use world::map::StaticMap;

const OPCODE_MAP_DESCRIPTION: u8 = 0x64;
const ANCHOR_DX: i32 = 8;
const ANCHOR_DY: i32 = 6;
const VIEWPORT_WIDTH: i32 = 18;
const VIEWPORT_HEIGHT: i32 = 14;

/// Build a one-tile map: ground (4526) + one always-on-top item (1059) + one
/// down item (2000) at (1000,1000,7). Expected wire stack: [4526, 1059, 2000],
/// with the creature slot after the first two (pre_creature_len = 2).
fn decorated_map() -> (OtbmMap, ItemsOtb) {
    let items = ItemsOtb {
        major_version: 3,
        minor_version: 57,
        build_number: 0,
        items: vec![
            ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0 },
            ItemType { group: 5, flags: 1 << 13, server_id: 200, client_id: 1059, always_on_top: true, top_order: 0 },
            ItemType { group: 5, flags: 0, server_id: 300, client_id: 2000, always_on_top: false, top_order: 0 },
        ],
    };
    let map = OtbmMap {
        width: 2000, height: 2000, major_items: 3, minor_items: 57,
        description: String::new(), spawn_file: None, house_file: None,
        tiles: vec![MapTile {
            x: 1000, y: 1000, z: 7, flags: 0, house_id: None,
            items: vec![
                MapItem { id: 100, contents: vec![] },
                MapItem { id: 200, contents: vec![] },
                MapItem { id: 300, contents: vec![] },
            ],
        }],
        towns: vec![Town { id: 1, name: "Thais".into(), x: 1000, y: 1000, z: 7 }],
        waypoints: vec![],
    };
    (map, items)
}

/// Minimal OTClient-faithful decoder: reads things per tile (each plain item is
/// `[clientId u16][0xFF]`) until a `>= 0xFF00` skip marker. No creatures here.
fn decode(bytes: &[u8], center: Center) -> HashMap<(i32, i32, i32), Vec<u16>> {
    assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);
    let mut p = 6usize;
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
                assert_eq!(peek, 0x0000, "env effects u16 at {p}");
                p += 2;
                let mut ids = Vec::new();
                loop {
                    let v = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
                    if v >= 0xFF00 {
                        skip = i32::from(v & 0x00FF);
                        p += 2;
                        break;
                    }
                    assert_eq!(bytes[p + 2], 0xFF, "item mark at {}", p + 2);
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

#[test]
fn static_map_stack_round_trips_through_encoder() {
    let (map, items) = decorated_map();
    let sm = StaticMap::from_formats(&map, &items);
    let center = Center { x: 1000, y: 1000, z: 7 };
    let bytes = encode(center, &sm, &[]);
    let found = decode(&bytes, center);
    assert_eq!(found.get(&(1000, 1000, 7)), Some(&vec![4526, 1059, 2000]));
}

#[test]
fn walk_update_uses_id_form_for_creature_move() {
    let (map, items) = decorated_map();
    let sm = StaticMap::from_formats(&map, &items);
    let id = 0x1000_0000u32;
    // The move must locate the creature by id, not by (oldPos, stackpos): the
    // server's items.otb stackpos can disagree with OTClient's .dat placement.
    let out = walk::walk_update(id, (1000, 1000, 7), (1001, 1000, 7), &sm, &[]);
    assert_eq!(out[0], 0x6D); // creature move opcode
    assert_eq!(u16::from_le_bytes([out[1], out[2]]), 0xFFFF); // id-form marker
    assert_eq!(u32::from_le_bytes([out[3], out[4], out[5], out[6]]), id);
    assert_eq!(u16::from_le_bytes([out[7], out[8]]), 1001); // new x
}
