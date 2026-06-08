//! Container protocol — open/close/update packets for bags and backpacks.
//!
//! Outbound (server → client):
//!   `0x6E` open container  (`sendContainer`)
//!   `0x6F` close container (`sendCloseContainer`)
//!   `0x70` add item        (`sendAddContainerItem`)
//!   `0x71` update item     (`sendUpdateContainerItem`)
//!   `0x72` remove item     (`sendRemoveContainerItem`)
//!
//! Inbound (client → server):
//!   `0x82` use item        (`parseUseItem`)
//!   `0x87` close container (`parseCloseContainer`)
//!   `0x88` up-arrow        (`parseUpArrowContainer`)
//!
//! All refs: `reference/tfs/src/protocolgame.cpp`.

use crate::map_description::WireItem;
use crate::message::{MessageReader, MessageWriter};
use crate::tile_item::write_item;

pub const OP_OPEN_CONTAINER: u8 = 0x6E;
pub const OP_CLOSE_CONTAINER: u8 = 0x6F;
pub const OP_ADD_CONTAINER_ITEM: u8 = 0x70;
pub const OP_UPDATE_CONTAINER_ITEM: u8 = 0x71;
pub const OP_REMOVE_CONTAINER_ITEM: u8 = 0x72;
pub const OP_USE_ITEM: u8 = 0x82;
pub const OP_CLOSE_CONTAINER_IN: u8 = 0x87;
pub const OP_UP_ARROW: u8 = 0x88;

/// Parsed inbound `0x82` use-item.
#[derive(Debug, Clone, Copy)]
pub struct UseItem {
    pub pos_x: u16,
    pub pos_y: u16,
    pub pos_z: u8,
    pub sprite_id: u16,
    pub stackpos: u8,
    /// Client-side container slot to open into (0 = any free slot).
    pub index: u8,
}

/// Parse an inbound `0x82` use-item body (bytes after the opcode).
/// Layout: `[x u16][y u16][z u8][spriteId u16][stackpos u8][index u8]`.
pub fn parse_use_item(body: &[u8]) -> Option<UseItem> {
    let mut r = MessageReader::new(body);
    let pos_x = r.read_u16().ok()?;
    let pos_y = r.read_u16().ok()?;
    let pos_z = r.read_u8().ok()?;
    let sprite_id = r.read_u16().ok()?;
    let stackpos = r.read_u8().ok()?;
    let index = r.read_u8().ok()?;
    Some(UseItem { pos_x, pos_y, pos_z, sprite_id, stackpos, index })
}

/// Parse an inbound `0x87` close-container or `0x88` up-arrow body: `[cid u8]`.
pub fn parse_container_cid(body: &[u8]) -> Option<u8> {
    MessageReader::new(body).read_u8().ok()
}

/// One item held inside a container, ready to encode onto the wire.
#[derive(Debug, Clone, Copy)]
pub struct ContainerWireItem {
    pub client_id: u16,
    /// Count byte for stackables; `None` for non-stackables.
    pub subtype: Option<u8>,
    pub animated: bool,
}

impl ContainerWireItem {
    pub fn as_wire(&self) -> WireItem {
        WireItem { client_id: self.client_id, subtype: self.subtype, animated: self.animated }
    }
}

/// `0x6E` open container.
///
/// `has_parent`: true when the container was opened from inside another
/// container (enables the up-arrow button in the client).
/// Normal (non-depot) containers use `is_unlocked=true`, `has_pagination=false`.
///
/// `bag` is the container item itself encoded as a wire item. Use `write_item`
/// so animated bags (e.g. backpack of holding) include the `0xFE` phase byte.
pub fn open_container(
    cid: u8,
    bag: &WireItem,
    name: &str,
    capacity: u8,
    has_parent: bool,
    items: &[ContainerWireItem],
) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_OPEN_CONTAINER);
    w.write_u8(cid);
    // Bag item: [client_id u16][0xFF mark][0xFE if animated] — via write_item.
    write_item(&mut w, bag);
    // Container name.
    w.write_string(name.as_bytes());
    // Capacity, parent flag, unlocked flag, pagination flag.
    w.write_u8(capacity);
    w.write_u8(u8::from(has_parent));
    w.write_u8(0x01); // is_unlocked: always true for normal bags
    w.write_u8(0x00); // has_pagination: always false for normal bags
    // Total and first index (no pagination for normal bags).
    let total = items.len().min(u16::MAX as usize) as u16;
    w.write_u16(total);
    w.write_u16(0x00); // first_index
    // Items to send: min(capacity, total, 255).
    let to_send = (capacity as usize).min(items.len()).min(255);
    w.write_u8(to_send as u8);
    for item in items.iter().take(to_send) {
        write_item(&mut w, &item.as_wire());
    }
    w.into_bytes()
}

/// `0x6F` close container (server-initiated, e.g. when the bag is dropped).
pub fn close_container(cid: u8) -> Vec<u8> {
    vec![OP_CLOSE_CONTAINER, cid]
}

/// `0x70` add an item at `slot` inside `cid`.
/// Slots are 0-based; TFS uses `u16` for the slot even though capacity is ≤ 255.
pub fn add_container_item(cid: u8, slot: u16, item: &ContainerWireItem) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_ADD_CONTAINER_ITEM);
    w.write_u8(cid);
    w.write_u16(slot);
    write_item(&mut w, &item.as_wire());
    w.into_bytes()
}

/// `0x71` update the item at `slot` inside `cid` (count changed).
pub fn update_container_item(cid: u8, slot: u16, item: &ContainerWireItem) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_UPDATE_CONTAINER_ITEM);
    w.write_u8(cid);
    w.write_u16(slot);
    write_item(&mut w, &item.as_wire());
    w.into_bytes()
}

/// `0x72` remove the item at `slot` inside `cid`.
///
/// `replacement`: when removing slot 0 the next item slides up; pass the new
/// slot-0 item so the client can render it correctly.  `None` means the
/// container is now empty (or the removed slot was the last one).
pub fn remove_container_item(cid: u8, slot: u16, replacement: Option<&ContainerWireItem>) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_REMOVE_CONTAINER_ITEM);
    w.write_u8(cid);
    w.write_u16(slot);
    match replacement {
        Some(item) => write_item(&mut w, &item.as_wire()),
        None => w.write_u16(0x0000),
    }
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_container_layout() {
        let items = [
            ContainerWireItem { client_id: 100, subtype: None, animated: false },
            ContainerWireItem { client_id: 200, subtype: Some(5), animated: false },
        ];
        let bag = WireItem { client_id: 1988, subtype: None, animated: false };
        let pkt = open_container(0, &bag, "backpack", 20, false, &items);
        assert_eq!(pkt[0], OP_OPEN_CONTAINER);
        assert_eq!(pkt[1], 0); // cid
        assert_eq!(u16::from_le_bytes([pkt[2], pkt[3]]), 1988); // client_id
        assert_eq!(pkt[4], 0xFF); // mark
        // name: u16 len + 8 bytes "backpack"
        let name_len = u16::from_le_bytes([pkt[5], pkt[6]]) as usize;
        assert_eq!(name_len, 8);
        let name = std::str::from_utf8(&pkt[7..7 + name_len]).unwrap();
        assert_eq!(name, "backpack");
        let base = 7 + name_len;
        assert_eq!(pkt[base], 20);   // capacity
        assert_eq!(pkt[base + 1], 0); // has_parent = false
        assert_eq!(pkt[base + 2], 1); // is_unlocked
        assert_eq!(pkt[base + 3], 0); // has_pagination
        assert_eq!(u16::from_le_bytes([pkt[base + 4], pkt[base + 5]]), 2); // total
        assert_eq!(u16::from_le_bytes([pkt[base + 6], pkt[base + 7]]), 0); // first_index
        assert_eq!(pkt[base + 8], 2); // items_to_send
    }

    #[test]
    fn open_container_animated_bag_includes_phase_byte() {
        // Animated containers (e.g. backpack of holding) must include 0xFE after
        // the 0xFF mark so OTClient parses the name at the correct offset.
        let bag = WireItem { client_id: 2872, subtype: None, animated: true };
        let pkt = open_container(1, &bag, "backpack of holding", 20, true, &[]);
        assert_eq!(pkt[0], OP_OPEN_CONTAINER);
        assert_eq!(pkt[1], 1); // cid
        assert_eq!(u16::from_le_bytes([pkt[2], pkt[3]]), 2872); // client_id
        assert_eq!(pkt[4], 0xFF); // mark
        assert_eq!(pkt[5], 0xFE); // animation phase (present for animated bags)
        // Name starts at byte 6.
        let name_len = u16::from_le_bytes([pkt[6], pkt[7]]) as usize;
        assert_eq!(name_len, 19); // "backpack of holding"
    }

    #[test]
    fn close_container_layout() {
        assert_eq!(close_container(3), [OP_CLOSE_CONTAINER, 3]);
    }

    #[test]
    fn add_update_remove_layouts() {
        let item = ContainerWireItem { client_id: 100, subtype: None, animated: false };
        let add = add_container_item(1, 0, &item);
        assert_eq!(add[0], OP_ADD_CONTAINER_ITEM);
        assert_eq!(add[1], 1); // cid
        assert_eq!(u16::from_le_bytes([add[2], add[3]]), 0); // slot
        assert_eq!(u16::from_le_bytes([add[4], add[5]]), 100); // client_id
        assert_eq!(add[6], 0xFF); // mark

        let upd = update_container_item(2, 3, &item);
        assert_eq!(upd[0], OP_UPDATE_CONTAINER_ITEM);
        assert_eq!(upd[1], 2);
        assert_eq!(u16::from_le_bytes([upd[2], upd[3]]), 3);

        let rem_with = remove_container_item(0, 0, Some(&item));
        assert_eq!(rem_with[0], OP_REMOVE_CONTAINER_ITEM);
        assert_eq!(rem_with[1], 0);
        assert_eq!(u16::from_le_bytes([rem_with[2], rem_with[3]]), 0); // slot
        assert_eq!(u16::from_le_bytes([rem_with[4], rem_with[5]]), 100); // client_id of replacement

        let rem_empty = remove_container_item(0, 0, None);
        assert_eq!(rem_empty[0], OP_REMOVE_CONTAINER_ITEM);
        // No replacement: writes u16 0x0000
        assert_eq!(u16::from_le_bytes([rem_empty[4], rem_empty[5]]), 0x0000);
    }

    #[test]
    fn parse_use_item_layout() {
        let mut body = Vec::new();
        body.extend_from_slice(&0xFFFFu16.to_le_bytes()); // x
        body.extend_from_slice(&3u16.to_le_bytes());       // y (slot 3)
        body.push(0);                                       // z
        body.extend_from_slice(&1988u16.to_le_bytes());    // sprite_id
        body.push(2);                                       // stackpos
        body.push(0);                                       // index
        let u = parse_use_item(&body).unwrap();
        assert_eq!(u.pos_x, 0xFFFF);
        assert_eq!(u.pos_y, 3);
        assert_eq!(u.pos_z, 0);
        assert_eq!(u.sprite_id, 1988);
        assert_eq!(u.stackpos, 2);
        assert_eq!(u.index, 0);
    }
}
