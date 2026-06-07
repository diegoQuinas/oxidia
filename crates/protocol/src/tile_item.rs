//! Item-on-tile wire forms. `0x6A` (add) and `0x6B` (update/transform) share the
//! opcodes used for creatures, namespaced by payload: here the trailing bytes are
//! an item (`[u16 client_id][u8 0xFF]` + optional count) rather than a creature.
//! Removal reuses `tile_creature::remove_tile_thing` (`0x6C`, positional).
//! Refs: `sendAddTileItem`/`sendUpdateTileItem` `protocolgame.cpp`.

use crate::map_description::WireItem;
use crate::message::MessageWriter;

const OP_ADD_TILE_THING: u8 = 0x6A;
const OP_UPDATE_TILE_THING: u8 = 0x6B;
const MARK_UNMARKED: u8 = 0xFF;

fn write_item(w: &mut MessageWriter, item: &WireItem) {
    w.write_u16(item.client_id);
    w.write_u8(MARK_UNMARKED);
    if let Some(subtype) = item.subtype {
        w.write_u8(subtype);
    }
    if item.animated {
        w.write_u8(0xFE);
    }
}

/// `0x6A` add an item to `(pos)` at `stackpos`: `[0x6A][pos][stackpos][item]`.
pub fn add_tile_item(pos: (u16, u16, u8), stackpos: u8, item: &WireItem) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_ADD_TILE_THING);
    w.write_u16(pos.0);
    w.write_u16(pos.1);
    w.write_u8(pos.2);
    w.write_u8(stackpos);
    write_item(&mut w, item);
    w.into_bytes()
}

/// `0x6B` replace the thing at `(pos, stackpos)` with `item` (e.g. a stackable
/// whose count changed): `[0x6B][pos][stackpos][item]`.
pub fn update_tile_item(pos: (u16, u16, u8), stackpos: u8, item: &WireItem) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_UPDATE_TILE_THING);
    w.write_u16(pos.0);
    w.write_u16(pos.1);
    w.write_u8(pos.2);
    w.write_u8(stackpos);
    write_item(&mut w, item);
    w.into_bytes()
}
