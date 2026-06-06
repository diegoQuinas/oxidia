//! Walk-related server->client packets for protocol 10.98.
//! Ports `sendMoveCreature` (0x6D + directional slices), `sendCancelWalk` (0xB5),
//! and `sendCreatureTurn` (0x6B) from `reference/tfs/src/protocolgame.cpp`.

use crate::map_description::{self, PlacedCreature, TileSource};
use crate::message::MessageWriter;

pub const OP_CREATURE_MOVE: u8 = 0x6D;
pub const OP_CANCEL_WALK: u8 = 0xB5;
pub const OP_CREATURE_TURN: u8 = 0x6B;

const SLICE_NORTH: u8 = 0x65;
const SLICE_EAST: u8 = 0x66;
const SLICE_SOUTH: u8 = 0x67;
const SLICE_WEST: u8 = 0x68;

const VIEW_X: i32 = 8; // Map::maxClientViewportX
const VIEW_Y: i32 = 6; // Map::maxClientViewportY
const SLICE_W: i32 = (VIEW_X * 2) + 2; // 18
const SLICE_H: i32 = (VIEW_Y * 2) + 2; // 14

/// `0x6D` creature move, **creature-id form**: `[0x6D][0xFFFF][creatureId u32][newPos]`.
///
/// The client locates the creature via `getCreatureById` (OTClient
/// `getMappedThing`, `x == 0xFFFF` branch) instead of by `(oldPos, stackPos)`.
/// This is deliberate: the server derives a tile's stackpos from `items.otb`
/// (`FLAG_ALWAYSONTOP`), but OTClient re-inserts a moved creature by its `.dat`
/// `getStackPriority`. When those two data sources disagree about whether a tile
/// item sits above or below the creature, the `(oldPos, stackPos)` form points the
/// client at the wrong thing and the move is silently dropped (observed live as
/// "no creature found to move" / "no thing at pos:…,stackpos:2"). The id form is
/// immune to that divergence. TFS itself uses it whenever stackpos >= 10
/// (`protocolgame.cpp:2603`), so it is protocol-legal for every move.
pub fn creature_move(id: u32, new: (u16, u16, u8)) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_MOVE);
    w.write_u16(0xFFFF);
    w.write_u32(id);
    add_position(&mut w, new);
    w.into_bytes()
}

/// `0xB5` cancel walk: snaps the client back and sets its facing.
pub fn cancel_walk(direction: u8) -> Vec<u8> {
    vec![OP_CANCEL_WALK, direction]
}

/// `0x6B` creature turn (`GameServerChangeOnMap` -> `parseTileTransformThing`),
/// **creature-id form**: `[0x6B][0xFFFF][id u32][0x0063][id u32][direction][walkthrough]`.
///
/// Like [`creature_move`], the leading `0xFFFF` makes the client locate the
/// existing creature via `getCreatureById` instead of `(pos, stackpos)`, so a
/// turn on a decorated tile is immune to the same items.otb-vs-`.dat` stackpos
/// divergence. The trailing `0x0063` block is the replacement creature thing the
/// client adds back, carrying the new facing.
pub fn creature_turn(id: u32, direction: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_TURN);
    w.write_u16(0xFFFF);
    w.write_u32(id);
    w.write_u16(0x0063);
    w.write_u32(id);
    w.write_u8(direction);
    w.write_u8(0x00); // walkthrough
    w.into_bytes()
}

/// Assemble the full server->client response for a same-floor step: the `0x6D`
/// move, then the newly-revealed row/column slice(s). Ports the independent y/x
/// `if` blocks of TFS `sendMoveCreature` (2616-2630) — a diagonal emits both.
pub fn walk_update<S: TileSource>(
    id: u32,
    old: (u16, u16, u8),
    new: (u16, u16, u8),
    src: &S,
    creatures: &[PlacedCreature],
) -> Vec<u8> {
    let mut out = creature_move(id, new);
    let (ox, oy) = (i32::from(old.0), i32::from(old.1));
    let (nx, ny) = (i32::from(new.0), i32::from(new.1));
    let nz = i32::from(new.2);

    if oy > ny {
        out.extend(map_description::encode_slice(
            SLICE_NORTH, ox - VIEW_X, ny - VIEW_Y, nz, SLICE_W, 1, src, creatures,
        ));
    } else if oy < ny {
        out.extend(map_description::encode_slice(
            SLICE_SOUTH, ox - VIEW_X, ny + (VIEW_Y + 1), nz, SLICE_W, 1, src, creatures,
        ));
    }
    if ox < nx {
        out.extend(map_description::encode_slice(
            SLICE_EAST, nx + (VIEW_X + 1), ny - VIEW_Y, nz, 1, SLICE_H, src, creatures,
        ));
    } else if ox > nx {
        out.extend(map_description::encode_slice(
            SLICE_WEST, nx - VIEW_X, ny - VIEW_Y, nz, 1, SLICE_H, src, creatures,
        ));
    }
    out
}

fn add_position(w: &mut MessageWriter, p: (u16, u16, u8)) {
    w.write_u16(p.0);
    w.write_u16(p.1);
    w.write_u8(p.2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::map_description::{TileSlices, WireItem};

    struct MapStub(HashMap<(i32, i32, i32), WireItem>);
    impl TileSource for MapStub {
        fn tile(&self, x: i32, y: i32, z: i32) -> Option<TileSlices<'_>> {
            self.0.get(&(x, y, z)).map(|item| TileSlices {
                pre_creature: std::slice::from_ref(item),
                post_creature: &[],
            })
        }
        fn creature_stackpos(&self, _x: i32, _y: i32, _z: i32) -> u8 {
            1
        }
    }

    #[test]
    fn creature_move_uses_id_form() {
        let p = creature_move(0x1000_0000, (101, 100, 7));
        assert_eq!(p[0], OP_CREATURE_MOVE);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 0xFFFF); // id-form marker
        assert_eq!(u32::from_le_bytes([p[3], p[4], p[5], p[6]]), 0x1000_0000); // creature id
        assert_eq!(u16::from_le_bytes([p[7], p[8]]), 101); // new x
        assert_eq!(u16::from_le_bytes([p[9], p[10]]), 100); // new y
        assert_eq!(p[11], 7); // new z
        assert_eq!(p.len(), 12);
    }

    #[test]
    fn cancel_walk_layout() {
        assert_eq!(cancel_walk(3), [OP_CANCEL_WALK, 3]);
    }

    #[test]
    fn creature_turn_uses_id_form() {
        let p = creature_turn(0x1000_0000, 1);
        assert_eq!(p[0], OP_CREATURE_TURN);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 0xFFFF); // id-form marker
        assert_eq!(u32::from_le_bytes([p[3], p[4], p[5], p[6]]), 0x1000_0000); // lookup id
        assert_eq!(u16::from_le_bytes([p[7], p[8]]), 0x0063); // replacement thing marker
        assert_eq!(u32::from_le_bytes([p[9], p[10], p[11], p[12]]), 0x1000_0000); // creature id
        assert_eq!(p[13], 1); // direction
        assert_eq!(p[14], 0); // walkthrough
        assert_eq!(p.len(), 15);
    }

    #[test]
    fn east_step_emits_move_then_east_slice() {
        let stub = MapStub(HashMap::new());
        let out = walk_update(0x1000_0000, (100, 100, 7), (101, 100, 7), &stub, &[]);
        assert_eq!(out[0], OP_CREATURE_MOVE);
        assert_eq!(out[12], SLICE_EAST);
    }

    #[test]
    fn northeast_step_emits_both_slices() {
        let stub = MapStub(HashMap::new());
        let out = walk_update(0x1000_0000, (100, 100, 7), (101, 99, 7), &stub, &[]);
        assert_eq!(out[0], OP_CREATURE_MOVE);
        assert!(out.contains(&SLICE_NORTH));
        assert!(out.contains(&SLICE_EAST));
    }
}
