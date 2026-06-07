//! `0x6A` (add a creature to a tile) and `0x6C` (remove a thing from a tile)
//! server->client packets for protocol 10.98. Byte-faithful ports of
//! `reference/tfs/src/protocolgame.cpp`: `sendAddCreature` (2517-2522) and
//! `RemoveTileThing` (3101-3109). Used by the M5 spectator broadcast when a
//! creature enters (`0x6A`) or leaves (`0x6C`) another player's viewport.

use crate::message::MessageWriter;

pub const OP_ADD_TILE_CREATURE: u8 = 0x6A;
pub const OP_REMOVE_TILE_THING: u8 = 0x6C;

/// `0x6A`: place a creature on a tile. `thing` is the creature serialized by
/// [`crate::creature::add_creature`] (the `0x61` full or `0x62` short form).
pub fn add_tile_creature(pos: (u16, u16, u8), stackpos: u8, thing: &[u8]) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_ADD_TILE_CREATURE);
    w.write_u16(pos.0);
    w.write_u16(pos.1);
    w.write_u8(pos.2);
    w.write_u8(stackpos);
    let mut out = w.into_bytes();
    out.extend_from_slice(thing);
    out
}

/// `0x6C`: remove the thing at `stackpos` from a tile (short form, stackpos < 10).
pub fn remove_tile_thing(pos: (u16, u16, u8), stackpos: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_REMOVE_TILE_THING);
    w.write_u16(pos.0);
    w.write_u16(pos.1);
    w.write_u8(pos.2);
    w.write_u8(stackpos);
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_tile_creature_layout() {
        let thing = [0xAA, 0xBB, 0xCC];
        let p = add_tile_creature((100, 200, 7), 1, &thing);
        assert_eq!(p[0], OP_ADD_TILE_CREATURE);
        assert_eq!(u16::from_le_bytes([p[1], p[2]]), 100); // x
        assert_eq!(u16::from_le_bytes([p[3], p[4]]), 200); // y
        assert_eq!(p[5], 7); // z
        assert_eq!(p[6], 1); // stackpos
        assert_eq!(&p[7..], &thing); // creature thing appended verbatim
        assert_eq!(p.len(), 7 + thing.len());
    }

    #[test]
    fn remove_tile_thing_layout() {
        let p = remove_tile_thing((100, 200, 7), 1);
        assert_eq!(p, [OP_REMOVE_TILE_THING, 100, 0, 200, 0, 7, 1]);
    }
}
