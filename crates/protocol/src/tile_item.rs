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

pub(crate) fn write_item(w: &mut MessageWriter, item: &WireItem) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `add_tile_item((1,2,7), 3, &WireItem::plain(100))` must produce:
    /// `[0x6A, 0x01,0x00, 0x02,0x00, 0x07, 0x03, 0x64,0x00, 0xFF]`
    /// — opcode, x LE, y LE, z, stackpos, client_id LE (100 = 0x64), MARK_UNMARKED 0xFF.
    #[test]
    fn add_tile_item_plain_item_layout() {
        let item = WireItem::plain(100);
        let pkt = add_tile_item((1, 2, 7), 3, &item);
        assert_eq!(pkt[0], OP_ADD_TILE_THING, "opcode 0x6A");
        assert_eq!(u16::from_le_bytes([pkt[1], pkt[2]]), 1, "x = 1");
        assert_eq!(u16::from_le_bytes([pkt[3], pkt[4]]), 2, "y = 2");
        assert_eq!(pkt[5], 7, "z = 7");
        assert_eq!(pkt[6], 3, "stackpos = 3");
        assert_eq!(u16::from_le_bytes([pkt[7], pkt[8]]), 100, "client_id 100");
        assert_eq!(pkt[9], 0xFF, "MARK_UNMARKED 0xFF");
        assert_eq!(pkt.len(), 10, "plain item: exactly 10 bytes");
    }

    /// A stackable item must append the count byte after MARK_UNMARKED.
    #[test]
    fn add_tile_item_stackable_appends_count_byte() {
        let item = WireItem { client_id: 100, subtype: Some(5), animated: false };
        let pkt = add_tile_item((1, 2, 7), 3, &item);
        // [0x6A, x_lo, x_hi, y_lo, y_hi, z, stackpos, cid_lo, cid_hi, 0xFF, count]
        assert_eq!(pkt[0], OP_ADD_TILE_THING);
        assert_eq!(pkt[9], 0xFF, "MARK_UNMARKED");
        assert_eq!(pkt[10], 5, "subtype/count byte = 5");
        assert_eq!(pkt.len(), 11, "stackable item: 11 bytes");
    }

    /// `update_tile_item` must lead with opcode `0x6B` (not `0x6A`).
    #[test]
    fn update_tile_item_uses_0x6b_opcode() {
        let item = WireItem::plain(200);
        let pkt = update_tile_item((10, 20, 7), 1, &item);
        assert_eq!(pkt[0], OP_UPDATE_TILE_THING, "opcode must be 0x6B");
        assert_ne!(pkt[0], OP_ADD_TILE_THING, "must not be 0x6A");
        assert_eq!(u16::from_le_bytes([pkt[1], pkt[2]]), 10, "x");
        assert_eq!(u16::from_le_bytes([pkt[3], pkt[4]]), 20, "y");
    }
}
