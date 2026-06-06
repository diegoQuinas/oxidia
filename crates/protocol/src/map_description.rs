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

/// Provides the ground item's client id at a world coordinate, or `None` if the
/// tile has no ground (empty / out of bounds).
pub trait GroundSource {
    fn ground(&self, x: i32, y: i32, z: i32) -> Option<u16>;
}

/// A position the encoder centers the viewport on.
#[derive(Debug, Clone, Copy)]
pub struct Center {
    pub x: u16,
    pub y: u16,
    pub z: u8,
}

/// Encode a full `0x64` map description centered on `center`.
/// M3 supports overground centers (z <= 7) only.
pub fn encode<S: GroundSource>(center: Center, src: &S) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OPCODE_MAP_DESCRIPTION);
    w.write_u16(center.x);
    w.write_u16(center.y);
    w.write_u8(center.z);
    write_tiles(&mut w, center, src);
    w.into_bytes()
}

fn write_tiles<S: GroundSource>(w: &mut MessageWriter, center: Center, src: &S) {
    let anchor_x = center.x as i32 - ANCHOR_DX;
    let anchor_y = center.y as i32 - ANCHOR_DY;

    // Exact port of TFS `GetMapDescription` + `GetFloorDescription`
    // (reference/tfs/src/protocolgame.cpp:633-680). `skip` persists across ALL
    // overground floors and starts at -1, so a stream that opens on a real tile
    // emits no leading skip pair. On an empty tile: flush `[0xFF][0xFF]` when the
    // run reaches 0xFE, otherwise increment. On a real tile: flush `[skip][0xFF]`
    // if a run is open, then write the tile. A final `[skip][0xFF]` closes the
    // last open run. The OTClient decoder is the exact mirror of this.
    let mut skip: i32 = -1;
    for nz in (0..=7i32).rev() {
        let offset = center.z as i32 - nz;
        for nx in 0..VIEWPORT_WIDTH {
            for ny in 0..VIEWPORT_HEIGHT {
                let wx = anchor_x + nx + offset;
                let wy = anchor_y + ny + offset;
                match src.ground(wx, wy, nz) {
                    Some(client_id) => {
                        if skip >= 0 {
                            w.write_u8(skip as u8);
                            w.write_u8(0xFF);
                        }
                        skip = 0;
                        w.write_u16(0x0000); // environmental effects placeholder
                        add_item(w, client_id);
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

/// Minimal item serialization for a ground tile: `[u16 clientId][u8 0xFF]`.
/// (Stackable count / animation phase are not needed for M3 ground.)
fn add_item(w: &mut MessageWriter, client_id: u16) {
    w.write_u16(client_id);
    w.write_u8(MARK_UNMARKED);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapStub(HashMap<(i32, i32, i32), u16>);
    impl GroundSource for MapStub {
        fn ground(&self, x: i32, y: i32, z: i32) -> Option<u16> {
            self.0.get(&(x, y, z)).copied()
        }
    }

    /// Decode the tile stream back into a {(wx,wy,nz)->client_id} map.
    ///
    /// This is a faithful port of OTClient's `setFloorDescription`
    /// (`protocolgameparse.cpp`) — the exact inverse of the TFS encoder. It walks
    /// the same flat sequence of 8*W*H = 2016 positions (floors 7->0, then nx, ny)
    /// carrying a `skip` counter that persists across floors:
    ///   - when `skip == 0`, peek a u16: if its value is >= 0xFF00 (high byte
    ///     0xFF) it's a `[count][0xFF]` marker → set `skip = count`; otherwise it's
    ///     a tile → read `[env u16][clientId u16][0xFF]`, place it, then read the
    ///     trailing `[count][0xFF]` marker and set `skip = count`;
    ///   - when `skip > 0`, the position is empty → decrement.
    ///
    /// Validating the encoder against THIS decoder proves it matches the real
    /// client, not an invented scheme.
    fn decode_stream(bytes: &[u8], center: Center) -> HashMap<(i32, i32, i32), u16> {
        assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);
        let mut p = 6usize; // skip opcode + u16 x + u16 y + u8 z
        let anchor_x = center.x as i32 - ANCHOR_DX;
        let anchor_y = center.y as i32 - ANCHOR_DY;
        let floor_size = VIEWPORT_WIDTH * VIEWPORT_HEIGHT; // 252
        let total = 8 * floor_size; // 2016
        let mut found = HashMap::new();
        let mut skip = 0i32;
        let mut g_idx = 0i32;
        while g_idx < total {
            if skip == 0 {
                let peek = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
                if peek >= 0xFF00 {
                    // [count][0xFF] marker — current position is empty.
                    skip = i32::from(peek & 0x00FF);
                    p += 2;
                } else {
                    // Tile at this position: [env u16][clientId u16][0xFF].
                    assert_eq!(peek, 0x0000, "tile env effects at {p}");
                    let client_id = u16::from_le_bytes([bytes[p + 2], bytes[p + 3]]);
                    assert_eq!(bytes[p + 4], MARK_UNMARKED);
                    p += 5;
                    let fi = g_idx / floor_size; // 0 => floor 7, 7 => floor 0
                    let nz = 7 - fi;
                    let offset = center.z as i32 - nz;
                    let t = g_idx % floor_size;
                    let nx = t / VIEWPORT_HEIGHT;
                    let ny = t % VIEWPORT_HEIGHT;
                    found.insert((anchor_x + nx + offset, anchor_y + ny + offset, nz), client_id);
                    // Trailing run marker that follows every tile.
                    assert_eq!(bytes[p + 1], 0xFF, "trailing run marker at {}", p + 1);
                    skip = i32::from(u16::from_le_bytes([bytes[p], bytes[p + 1]]) & 0x00FF);
                    p += 2;
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
        let stub = MapStub(HashMap::new());
        let bytes = encode(Center { x: 1000, y: 1000, z: 7 }, &stub);
        assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);
        assert_eq!(u16::from_le_bytes([bytes[1], bytes[2]]), 1000);
        assert_eq!(u16::from_le_bytes([bytes[3], bytes[4]]), 1000);
        assert_eq!(bytes[5], 7);
    }

    #[test]
    fn empty_map_is_only_skip_flushes() {
        let stub = MapStub(HashMap::new());
        let bytes = encode(Center { x: 1000, y: 1000, z: 7 }, &stub);
        let found = decode_stream(&bytes, Center { x: 1000, y: 1000, z: 7 });
        assert!(found.is_empty());
    }

    #[test]
    fn single_ground_tile_at_center_round_trips() {
        let center = Center { x: 1000, y: 1000, z: 7 };
        let mut m = HashMap::new();
        m.insert((1000, 1000, 7), 4526u16);
        let stub = MapStub(m);
        let bytes = encode(center, &stub);
        let found = decode_stream(&bytes, center);
        assert_eq!(found.get(&(1000, 1000, 7)), Some(&4526));
        assert_eq!(found.len(), 1);
    }
}
