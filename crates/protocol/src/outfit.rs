//! Outfit packets for protocol 10.98.
//!
//! Inbound:
//!   - `0xD3` set outfit: `[u16 lookType][u8 head][u8 body][u8 legs][u8 feet]
//!     [u8 addons][u16 mount]` (body after opcode). 9 bytes.
//!
//! Outbound:
//!   - `0x8E` creature outfit: `[0x8E][u32 id][AddOutfit]`.
//!   - `0xC8` outfit window: `[0xC8][AddOutfit current][u8 outfitCount]
//!     {u16 lookType, string name, u8 addons}…[u8 mountCount]
//!     {u16 clientId, string name}…`.
//!
//! Sources verified against `reference/tfs/src/protocolgame.cpp`:
//!   - `parseSetOutfit`      line 829–840
//!   - `sendCreatureOutfit`  line 1252–1263
//!   - `sendOutfitWindow`    line 2783–2846
//!
//! All three reuse [`crate::creature::add_outfit`] (the 9-byte `AddOutfit`).

use crate::creature::{add_outfit, Outfit};
use crate::message::MessageWriter;

// ---------------------------------------------------------------------------
// Opcode constants
// ---------------------------------------------------------------------------

/// Inbound: client requests the outfit-selection window
/// (`playerRequestOutfit`, TFS `parseRecv` case 0xD2, `protocolgame.cpp:553`).
pub const OP_REQUEST_OUTFIT: u8 = 0xD2;

/// Inbound: client commits a new outfit (`parseSetOutfit`, TFS line 829).
pub const OP_SET_OUTFIT: u8 = 0xD3;

/// Outbound: a creature's outfit changed (`sendCreatureOutfit`, TFS line 1252).
pub const OP_CREATURE_OUTFIT: u8 = 0x8E;

/// Outbound: the outfit-selection window (`sendOutfitWindow`, TFS line 2783).
pub const OP_OUTFIT_WINDOW: u8 = 0xC8;

// ---------------------------------------------------------------------------
// Inbound parser
// ---------------------------------------------------------------------------

/// Parse the body of an inbound `0xD3` packet (the bytes **after** the opcode
/// byte, which the reader loop has already consumed).
///
/// Returns `Some(Outfit)` on success. Returns `None` if the body is shorter
/// than the fixed 9-byte layout.
pub fn parse_set_outfit(body: &[u8]) -> Option<Outfit> {
    if body.len() < 9 {
        return None;
    }
    Some(Outfit {
        look_type: u16::from_le_bytes([body[0], body[1]]),
        head: body[2],
        body: body[3],
        legs: body[4],
        feet: body[5],
        addons: body[6],
        mount: u16::from_le_bytes([body[7], body[8]]),
    })
}

// ---------------------------------------------------------------------------
// Outbound builders
// ---------------------------------------------------------------------------

/// Build a `0x8E` creature-outfit packet: `[0x8E][u32 id][AddOutfit]`.
pub fn creature_outfit(creature_id: u32, o: &Outfit) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_OUTFIT);
    w.write_u32(creature_id);
    add_outfit(&mut w, o);
    w.into_bytes()
}

/// One selectable outfit in the `0xC8` window catalog.
pub struct AvailableOutfit<'a> {
    pub look_type: u16,
    pub name: &'a [u8],
    pub addons: u8,
}

/// One owned mount in the `0xC8` window catalog.
pub struct AvailableMount<'a> {
    pub client_id: u16,
    pub name: &'a [u8],
}

/// Build a `0xC8` outfit-window packet.
///
/// Layout: `[0xC8][AddOutfit current][u8 outfitCount]{u16 lookType, string
/// name, u8 addons}…[u8 mountCount]{u16 clientId, string name}…`.
///
/// The client cannot display more than 255 of either list, so both counts are
/// clamped to 255 and any overflow entries are dropped (matching the TFS
/// `numeric_limits<uint8_t>::max()` break in `sendOutfitWindow`).
pub fn outfit_window(
    current: &Outfit,
    outfits: &[AvailableOutfit],
    mounts: &[AvailableMount],
) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_OUTFIT_WINDOW);
    add_outfit(&mut w, current);

    let outfit_count = outfits.len().min(255);
    w.write_u8(outfit_count as u8);
    for o in &outfits[..outfit_count] {
        w.write_u16(o.look_type);
        w.write_string(o.name);
        w.write_u8(o.addons);
    }

    let mount_count = mounts.len().min(255);
    w.write_u8(mount_count as u8);
    for m in &mounts[..mount_count] {
        w.write_u16(m.client_id);
        w.write_string(m.name);
    }

    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knight() -> Outfit {
        Outfit { look_type: 128, head: 78, body: 69, legs: 58, feet: 76, addons: 0, mount: 0 }
    }

    #[test]
    fn op_request_outfit_is_0xd2() {
        assert_eq!(OP_REQUEST_OUTFIT, 0xD2);
    }

    #[test]
    fn creature_outfit_opcode_id_then_outfit() {
        let bytes = creature_outfit(0x1000_0000, &knight());
        let mut p = 0;
        assert_eq!(bytes[p], OP_CREATURE_OUTFIT); p += 1;
        assert_eq!(u32::from_le_bytes([bytes[p], bytes[p+1], bytes[p+2], bytes[p+3]]), 0x1000_0000); p += 4;
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 128); // outfit looktype
        // 1 opcode + 4 id + 9 outfit = 14 bytes
        assert_eq!(bytes.len(), 14);
    }

    #[test]
    fn outfit_window_full_layout() {
        let outfits = [
            AvailableOutfit { look_type: 128, name: b"Citizen", addons: 0 },
            AvailableOutfit { look_type: 129, name: b"Hunter", addons: 3 },
        ];
        let mounts = [AvailableMount { client_id: 387, name: b"Widow Queen" }];
        let bytes = outfit_window(&knight(), &outfits, &mounts);

        let mut p = 0;
        assert_eq!(bytes[p], OP_OUTFIT_WINDOW); p += 1;
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 128); p += 9; // current outfit (9 bytes)

        assert_eq!(bytes[p], 2); p += 1; // outfit count
        // outfit 0
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 128); p += 2;
        let n0 = u16::from_le_bytes([bytes[p], bytes[p + 1]]) as usize; p += 2;
        assert_eq!(&bytes[p..p + n0], b"Citizen"); p += n0;
        assert_eq!(bytes[p], 0); p += 1; // addons
        // outfit 1
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 129); p += 2;
        let n1 = u16::from_le_bytes([bytes[p], bytes[p + 1]]) as usize; p += 2;
        assert_eq!(&bytes[p..p + n1], b"Hunter"); p += n1;
        assert_eq!(bytes[p], 3); p += 1; // addons

        assert_eq!(bytes[p], 1); p += 1; // mount count
        assert_eq!(u16::from_le_bytes([bytes[p], bytes[p + 1]]), 387); p += 2;
        let m0 = u16::from_le_bytes([bytes[p], bytes[p + 1]]) as usize; p += 2;
        assert_eq!(&bytes[p..p + m0], b"Widow Queen"); p += m0;

        assert_eq!(p, bytes.len(), "no trailing bytes");
    }

    #[test]
    fn outfit_window_empty_catalog() {
        let bytes = outfit_window(&knight(), &[], &[]);
        // 1 opcode + 9 outfit + 1 outfitCount(0) + 1 mountCount(0) = 12 bytes
        assert_eq!(bytes.len(), 12);
        assert_eq!(bytes[10], 0); // outfit count
        assert_eq!(bytes[11], 0); // mount count
    }

    #[test]
    fn outfit_window_clamps_catalog_to_255() {
        let many: Vec<AvailableOutfit> =
            (0..300).map(|i| AvailableOutfit { look_type: i, name: b"x", addons: 0 }).collect();
        let bytes = outfit_window(&knight(), &many, &[]);
        // count byte sits right after opcode (1) + current outfit (9) = index 10
        assert_eq!(bytes[10], 255, "count byte must clamp to 255, not wrap");
        // each entry is u16(2) + string(u16 len 2 + "x" 1) + addon(1) = 6 bytes
        // 1 opcode + 9 outfit + 1 count + 255*6 entries + 1 mountCount = 1542
        assert_eq!(bytes.len(), 1 + 9 + 1 + 255 * 6 + 1);
    }

    #[test]
    fn parse_set_outfit_reads_full_layout() {
        // lookType=128, head=78, body=69, legs=58, feet=76, addons=3, mount=1024
        let body = [128, 0, 78, 69, 58, 76, 3, 0, 4];
        let o = parse_set_outfit(&body).expect("9-byte body parses");
        assert_eq!(o.look_type, 128);
        assert_eq!(o.head, 78);
        assert_eq!(o.body, 69);
        assert_eq!(o.legs, 58);
        assert_eq!(o.feet, 76);
        assert_eq!(o.addons, 3);
        assert_eq!(o.mount, 1024);
    }

    #[test]
    fn parse_set_outfit_rejects_short_body() {
        assert!(parse_set_outfit(&[128, 0, 78, 69, 58, 76, 3, 0]).is_none());
    }
}
