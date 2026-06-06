//! `0x64` map description for protocol 10.98.
//! Mirrors `reference/tfs/src/protocolgame.cpp` (`GetMapDescription`/`GetFloorDescription`).
//! Viewport is 18 wide x 14 tall; overground walks floors 7->0. Empty tiles are
//! run-length "skip"-encoded: `[u8 skip][u8 0xFF]` flushes a run; `[0xFF][0xFF]`
//! flushes a full run of 255.

use crate::message::MessageWriter;

pub const OPCODE_MAP_DESCRIPTION: u8 = 0x64;
pub const MARK_UNMARKED: u8 = 0xFF;

pub const VIEWPORT_WIDTH: i32 = 18;
pub const VIEWPORT_HEIGHT: i32 = 14;
const ANCHOR_DX: i32 = 8; // (VIEWPORT_WIDTH / 2) - 1
const ANCHOR_DY: i32 = 6; // (VIEWPORT_HEIGHT / 2) - 1

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

/// A position the encoder centers the viewport on.
#[derive(Debug, Clone, Copy)]
pub struct Center {
    pub x: u16,
    pub y: u16,
    pub z: u8,
}

/// A creature already serialized via `crate::creature::add_creature`, placed at a
/// world coordinate. Spliced into the tile stream after that tile's ground item.
#[derive(Debug, Clone)]
pub struct PlacedCreature {
    pub x: u16,
    pub y: u16,
    pub z: u8,
    pub bytes: Vec<u8>,
}

/// Encode a full `0x64` map description centered on `center`, with `creatures`
/// rendered on their tiles. Writes the full tile stack (ground + top items, then
/// creatures, then down items) capped at 10 things. Overground centers (z <= 7) only.
pub fn encode<S: TileSource>(center: Center, src: &S, creatures: &[PlacedCreature]) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OPCODE_MAP_DESCRIPTION);
    w.write_u16(center.x);
    w.write_u16(center.y);
    w.write_u8(center.z);
    get_map_description(
        &mut w,
        center.x as i32 - ANCHOR_DX,
        center.y as i32 - ANCHOR_DY,
        center.z as i32,
        VIEWPORT_WIDTH,
        VIEWPORT_HEIGHT,
        src,
        creatures,
    );
    w.into_bytes()
}

/// Encode a directional map slice (`0x65`/`0x66`/`0x67`/`0x68`): just the opcode
/// followed by a `width`x`height` tile stream at `(anchor_x, anchor_y)`.
#[allow(clippy::too_many_arguments)]
pub fn encode_slice<S: TileSource>(
    opcode: u8,
    anchor_x: i32,
    anchor_y: i32,
    center_z: i32,
    width: i32,
    height: i32,
    src: &S,
    creatures: &[PlacedCreature],
) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(opcode);
    get_map_description(&mut w, anchor_x, anchor_y, center_z, width, height, src, creatures);
    w.into_bytes()
}

/// Exact port of TFS `GetMapDescription` + `GetFloorDescription`
/// (protocolgame.cpp:633-680). Floors 7->0, `skip` persists across them and
/// starts at -1. After a tile's pre_creature items, any creature on that tile is
/// spliced in, then post_creature (down) items follow. The full tile stack
/// (ground + top items, creatures, down items) is capped at 10 things total.
///
/// Skip-encoding: `skip` persists across ALL overground floors and starts at -1,
/// so a stream that opens on a real tile emits no leading skip pair. On an empty
/// tile: flush `[0xFF][0xFF]` when the run reaches 0xFE, otherwise increment. On
/// a real tile: flush `[skip][0xFF]` if a run is open, then write the tile. A
/// final `[skip][0xFF]` closes the last open run. The OTClient decoder is the
/// exact mirror of this.
#[allow(clippy::too_many_arguments)]
fn get_map_description<S: TileSource>(
    w: &mut MessageWriter,
    anchor_x: i32,
    anchor_y: i32,
    center_z: i32,
    width: i32,
    height: i32,
    src: &S,
    creatures: &[PlacedCreature],
) {
    let mut skip: i32 = -1;
    for nz in (0..=7i32).rev() {
        let offset = center_z - nz;
        for nx in 0..width {
            for ny in 0..height {
                let wx = anchor_x + nx + offset;
                let wy = anchor_y + ny + offset;
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
            }
        }
    }
    if skip >= 0 {
        w.write_u8(skip as u8);
        w.write_u8(0xFF);
    }
}

/// Serialize one tile item: `[u16 clientId][u8 0xFF]`. Used for every item in a
/// tile stack (ground, top, and down items). Stackable counts and animation
/// phases are not encoded yet — only plain items are sent.
fn add_item(w: &mut MessageWriter, client_id: u16) {
    w.write_u16(client_id);
    w.write_u8(MARK_UNMARKED);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    /// Decode the tile stream back into a {(wx,wy,nz)->Vec<client_id>} map.
    ///
    /// This is a faithful port of OTClient's `setFloorDescription`
    /// (`protocolgameparse.cpp`) — the exact inverse of the TFS encoder. It walks
    /// the same flat sequence of 8*W*H = 2016 positions (floors 7->0, then nx, ny)
    /// carrying a `skip` counter that persists across floors:
    ///   - when `skip == 0`, peek a u16: if its value is >= 0xFF00 (high byte
    ///     0xFF) it's a `[count][0xFF]` marker → set `skip = count`; otherwise it's
    ///     a tile → read `[env u16]` then things until next >= 0xFF00 marker;
    ///   - when `skip > 0`, the position is empty → decrement.
    ///
    /// Validating the encoder against THIS decoder proves it matches the real
    /// client, not an invented scheme.
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

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

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
                    // Tile: [env u16] then things until the next >= 0xFF00 marker.
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
                    let offset = center_z - nz;
                    let t = g_idx % floor_size;
                    let nx = t / height;
                    let ny = t % height;
                    found.insert(
                        (anchor_x + nx + offset, anchor_y + ny + offset, nz),
                        ids,
                    );
                }
            } else {
                skip -= 1;
            }
            g_idx += 1;
        }
        found
    }

    #[test]
    fn header_carries_center_position() {
        let stub = MapStub::ground_only(HashMap::new());
        let bytes = encode(Center { x: 1000, y: 1000, z: 7 }, &stub, &[]);
        assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);
        assert_eq!(u16::from_le_bytes([bytes[1], bytes[2]]), 1000);
        assert_eq!(u16::from_le_bytes([bytes[3], bytes[4]]), 1000);
        assert_eq!(bytes[5], 7);
    }

    #[test]
    fn empty_map_is_only_skip_flushes() {
        let stub = MapStub::ground_only(HashMap::new());
        let bytes = encode(Center { x: 1000, y: 1000, z: 7 }, &stub, &[]);
        let found = decode_stream(&bytes, Center { x: 1000, y: 1000, z: 7 });
        assert!(found.is_empty());
    }

    #[test]
    fn single_ground_tile_at_center_round_trips() {
        let center = Center { x: 1000, y: 1000, z: 7 };
        let mut m = HashMap::new();
        m.insert((1000, 1000, 7), 4526u16);
        let stub = MapStub::ground_only(m);
        let bytes = encode(center, &stub, &[]);
        let found = decode_stream(&bytes, center);
        assert_eq!(found.get(&(1000, 1000, 7)), Some(&vec![4526u16]));
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn creature_bytes_follow_the_center_ground_item() {
        let center = Center { x: 1000, y: 1000, z: 7 };
        let mut m = HashMap::new();
        m.insert((1000, 1000, 7), 4526u16);
        let stub = MapStub::ground_only(m);
        let creature = PlacedCreature {
            x: 1000,
            y: 1000,
            z: 7,
            bytes: vec![0x61, 0x00, 0xAA, 0xBB],
        };
        let bytes = encode(center, &stub, std::slice::from_ref(&creature));
        let ground = [0x00, 0x00, 0xAE, 0x11, 0xFF]; // 4526 = 0x11AE
        let gi = find_subsequence(&bytes, &ground).expect("ground present");
        assert_eq!(
            &bytes[gi + ground.len()..gi + ground.len() + 4],
            &creature.bytes[..]
        );
    }

    #[test]
    fn slice_round_trips_a_single_row() {
        let z: i32 = 7;
        let mut m = HashMap::new();
        m.insert((1005, 994, z), 4526u16);
        let stub = MapStub::ground_only(m);
        let bytes = encode_slice(0x65, 1000 - 8, 994, z, 18, 1, &stub, &[]);
        assert_eq!(bytes[0], 0x65);
        let found = decode_slice(&bytes[1..], 1000 - 8, 994, z, 18, 1);
        assert_eq!(found.get(&(1005, 994, z)), Some(&vec![4526u16]));
    }

    #[test]
    fn multi_item_tile_round_trips_in_wire_order() {
        let center = Center { x: 1000, y: 1000, z: 7 };
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
        let ids: Vec<u16> = (1..=12u16).collect();
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
        let mut stacks = HashMap::new();
        stacks.insert((1000, 1000, 7), (vec![4526u16, 1059u16, 2000u16], 2usize));
        let stub = MapStub { stacks };
        let creature = PlacedCreature { x: 1000, y: 1000, z: 7, bytes: vec![0x61, 0x00, 0xAA, 0xBB] };
        let bytes = encode(center, &stub, std::slice::from_ref(&creature));
        let top = [0x23, 0x04, 0xFF];   // 1059 = 0x0423
        let down = [0xD0, 0x07, 0xFF];  // 2000 = 0x07D0
        let ti = find_subsequence(&bytes, &top).expect("top item present");
        let ci = find_subsequence(&bytes, &creature.bytes).expect("creature present");
        let di = find_subsequence(&bytes, &down).expect("down item present");
        assert!(ti < ci, "creature after top item");
        assert!(ci < di, "creature before down item");
    }
}
