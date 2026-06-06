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

    // Overground: floors 7 down to 0.
    // `skip` counts empties since the last tile (or stream start).
    // Encoding: [skip][0xFF] before each tile, [0xFF][0xFF] every 255 empties.
    // The decoder advances its position by `skip` before placing the tile, so
    // skip == number_of_empties_since_last_event (0 = adjacent tiles).
    let mut skip: i32 = 0;
    for nz in (0..=7i32).rev() {
        let offset = center.z as i32 - nz;
        for nx in 0..VIEWPORT_WIDTH {
            for ny in 0..VIEWPORT_HEIGHT {
                let wx = anchor_x + nx + offset;
                let wy = anchor_y + ny + offset;
                match src.ground(wx, wy, nz) {
                    Some(client_id) => {
                        w.write_u8(skip as u8);
                        w.write_u8(0xFF);
                        skip = 0;
                        w.write_u16(0x0000); // environmental effects placeholder
                        add_item(w, client_id);
                    }
                    None => {
                        skip += 1;
                        if skip == 0xFF {
                            w.write_u8(0xFF);
                            w.write_u8(0xFF);
                            skip = 0;
                        }
                    }
                }
            }
        }
    }
    w.write_u8(skip as u8);
    w.write_u8(0xFF);
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

    /// Decode the tile stream back into a {(wx,wy,nz)->client_id} map so we can
    /// assert correctness without hand-computing 1900+ skip bytes.
    ///
    /// The encoder produces a single flat stream across all 8 overground floors
    /// (matching TFS's `GetMapDescription`): the skip counter is NOT reset between
    /// floors. Total tiles = 8 * W * H = 2016. A global index g_idx maps to
    /// floor/nx/ny as: floor_idx = g_idx / (W*H), nz = 7 - floor_idx,
    /// t = g_idx % (W*H), nx = t / H, ny = t % H.
    fn decode_stream(bytes: &[u8], center: Center) -> HashMap<(i32, i32, i32), u16> {
        assert_eq!(bytes[0], OPCODE_MAP_DESCRIPTION);
        let mut p = 6usize; // skip opcode + u16 x + u16 y + u8 z
        let anchor_x = center.x as i32 - ANCHOR_DX;
        let anchor_y = center.y as i32 - ANCHOR_DY;
        let mut found = HashMap::new();
        let total = 8 * VIEWPORT_WIDTH * VIEWPORT_HEIGHT; // 2016
        let floor_size = VIEWPORT_WIDTH * VIEWPORT_HEIGHT; // 252
        let mut g_idx = 0i32; // global tile index across all floors
        while g_idx < total && p < bytes.len() {
            let b0 = bytes[p];
            let b1 = bytes[p + 1];
            if b1 == 0xFF {
                let run = if b0 == 0xFF { 255 } else { b0 as i32 };
                g_idx += run;
                p += 2;
                if g_idx >= total || p >= bytes.len() {
                    break;
                }
                // After accumulating skips, check if the next bytes are another
                // skip flush ([0xFF][0xFF] or [n][0xFF]) or a tile.
                // A tile starts with [env_lo][env_hi] where env = 0x0000 so b1 != 0xFF.
                // Keep consuming skips until we reach a tile or end of stream.
                // (The [0xFF][0xFF] flush from the overflow guard doesn't precede a tile.)
                // Peek: if next two bytes are a skip pair, loop back.
                // A skip pair: bytes[p+1] == 0xFF. A tile: bytes[p+1] != 0xFF (env hi byte is 0x00).
                if bytes.get(p + 1).copied() == Some(0xFF) {
                    // Another skip flush — loop back to accumulate
                    continue;
                }
                // Now we're at a tile: [env_u16][clientId_u16][0xFF]
                let fi = g_idx / floor_size;  // floor index (0=floor7, 7=floor0)
                let nz = 7 - fi;
                let offset = center.z as i32 - nz;
                let t = g_idx % floor_size;
                let nx = t / VIEWPORT_HEIGHT;
                let ny = t % VIEWPORT_HEIGHT;
                let env = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
                assert_eq!(env, 0x0000);
                let client_id = u16::from_le_bytes([bytes[p + 2], bytes[p + 3]]);
                assert_eq!(bytes[p + 4], MARK_UNMARKED);
                found.insert((anchor_x + nx + offset, anchor_y + ny + offset, nz), client_id);
                p += 5;
                g_idx += 1;
            } else {
                panic!("unexpected stream byte at {p}: {b0:#04x} {b1:#04x}");
            }
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
