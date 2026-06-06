//! Decisive byte-level check: encode the real spawn viewport and decode it with a
//! client-faithful parser that reads the SAME conditional per-item bytes OTClient
//! reads (mark, count for stackable, fluid byte, animation phase) — derived from
//! items.otb. Full alignment (0 leftover, no over-read) proves the wire is sound.

use std::collections::HashMap;
use formats::otb::{self, ItemType};
use formats::otbm;
use protocol::map_description::{encode, Center, OPCODE_MAP_DESCRIPTION};
use world::map::StaticMap;

const FLAG_STACKABLE: u32 = 1 << 7;
const FLAG_ANIMATION: u32 = 1 << 24;

#[test]
fn real_spawn_viewport_aligns_with_client_parser() {
    let items_bytes = match std::fs::read("../../reference/tfs/data/items/items.otb") {
        Ok(b) => b,
        Err(_) => { eprintln!("skip: no reference items.otb"); return; }
    };
    let map_bytes = match std::fs::read("../../reference/tfs/data/world/forgotten.otbm") {
        Ok(b) => b,
        Err(_) => { eprintln!("skip: no reference map"); return; }
    };
    let items = otb::parse(&items_bytes).unwrap();
    let map = otbm::parse(&map_bytes).unwrap();

    // client-side attribute lookup by client_id (mimics the .dat via items.otb)
    let mut by_cid: HashMap<u16, &ItemType> = HashMap::new();
    for it in &items.items { by_cid.entry(it.client_id).or_insert(it); }

    let sm = StaticMap::from_formats(&map, &items);
    let spawn = sm.spawn();
    let center = Center { x: spawn.x, y: spawn.y, z: spawn.z };
    let bytes = encode(center, &sm, &[]);
    assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);

    // client-faithful decode (no creatures in this stream)
    let mut p = 6usize;
    let w = 18i32; let h = 14i32;
    let total = 8 * w * h;
    let mut skip = 0i32;
    let mut g = 0i32;
    let mut over = false;
    while g < total {
        if skip == 0 {
            if p + 1 >= bytes.len() { over = true; break; }
            let peek = u16::from_le_bytes([bytes[p], bytes[p+1]]);
            if peek >= 0xFF00 { skip = i32::from(peek & 0xFF); p += 2; }
            else {
                // env effect
                p += 2;
                loop {
                    if p + 1 >= bytes.len() { over = true; break; }
                    let v = u16::from_le_bytes([bytes[p], bytes[p+1]]);
                    if v >= 0xFF00 { skip = i32::from(v & 0xFF); p += 2; break; }
                    // item: id u16 + mark u8 (+count/fluid)(+phase)
                    p += 3; // id + mark
                    if let Some(it) = by_cid.get(&v) {
                        if it.flags & FLAG_STACKABLE != 0 || it.group == 11 || it.group == 12 { p += 1; }
                        if it.flags & FLAG_ANIMATION != 0 { p += 1; }
                    }
                }
                if over { break; }
            }
        } else { skip -= 1; }
        g += 1;
    }
    assert!(!over, "client parser over-read (EOF) — wire still misaligned at p={p}/{}", bytes.len());
    let leftover = bytes.len() as i64 - p as i64;
    assert_eq!(leftover, 0, "leftover bytes after client-faithful parse: {leftover}");
}
