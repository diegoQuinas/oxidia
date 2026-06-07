//! Look-at (examine) wire forms.
//!
//! Inbound `0x8C` (look at a tile thing) and `0x8D` (look in battle list); the
//! outbound reply is a `0xB4` text message of type `MESSAGE_INFO_DESCR`. The
//! text itself is assembled by `world` (it needs item metadata) — this module is
//! pure wire. Refs: `protocolgame.cpp:908` (parseLookAt), `:916`
//! (parseLookInBattleList), `const.h:191` (`MESSAGE_INFO_DESCR = 22`).

use crate::message::{MessageReader, MessageWriter};

/// TFS `MESSAGE_INFO_DESCR = 22` (`const.h:191`): green look-description message.
pub const MSG_INFO_DESCR: u8 = 22;

/// Parse inbound `0x8C` body (everything after the opcode byte):
/// `[x u16][y u16][z u8][spriteId u16, ignored][stackpos u8]`.
/// Returns `(x, y, z, stackpos)`, or `None` if the body is malformed.
pub fn parse_look(body: &[u8]) -> Option<(u16, u16, u8, u8)> {
    let mut r = MessageReader::new(body);
    let x = r.read_u16().ok()?;
    let y = r.read_u16().ok()?;
    let z = r.read_u8().ok()?;
    let _sprite = r.read_u16().ok()?; // spriteId, ignored (server resolves by stackpos)
    let stackpos = r.read_u8().ok()?;
    Some((x, y, z, stackpos))
}

/// Parse inbound `0x8D` body: `[creatureId u32]`. Returns the id or `None`.
pub fn parse_look_battle(body: &[u8]) -> Option<u32> {
    MessageReader::new(body).read_u32().ok()
}

/// Encode an outbound `0xB4 MESSAGE_INFO_DESCR` text message:
/// `[0xB4][22][u16 len][bytes]`. The string is Latin-1 bytes; over-255 is
/// truncated (documented divergence, same as chat).
pub fn info_descr(text: &[u8]) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(0xB4);
    w.write_u8(MSG_INFO_DESCR);
    w.write_string(&text[..text.len().min(255)]);
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_look` must round-trip (x, y, z, stackpos) and skip the 2-byte
    /// spriteId that sits between z and stackpos on the wire.
    #[test]
    fn parse_look_round_trips_and_skips_sprite_id() {
        // Wire layout: [x u16 LE][y u16 LE][z u8][spriteId u16 LE][stackpos u8]
        // Use a spriteId value distinct from x/y so a wrong offset is caught.
        let x: u16 = 1000;
        let y: u16 = 2000;
        let z: u8 = 7;
        let sprite_id: u16 = 0xABCD; // distinct from coords
        let stackpos: u8 = 3;
        let mut body = Vec::new();
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        body.push(z);
        body.extend_from_slice(&sprite_id.to_le_bytes());
        body.push(stackpos);
        let result = parse_look(&body).expect("valid body must parse");
        assert_eq!(result, (x, y, z, stackpos));
    }

    #[test]
    fn parse_look_returns_none_on_truncated_body() {
        // A body that is too short (only 4 bytes instead of the required 7)
        // must return None instead of panicking.
        let body = [0x01, 0x00, 0x02, 0x00]; // just x and y, missing z/sprite/stackpos
        assert!(parse_look(&body).is_none());
    }

    #[test]
    fn parse_look_battle_reads_creature_id() {
        let creature_id: u32 = 0x1234_5678;
        let body = creature_id.to_le_bytes();
        let result = parse_look_battle(&body).expect("valid body must parse");
        assert_eq!(result, creature_id);
    }

    #[test]
    fn info_descr_encodes_opcode_type_and_length_prefixed_string() {
        // Expected layout: [0xB4][22][u16 LE len][bytes]
        // For "hi" (2 bytes): [0xB4, 22, 0x02, 0x00, b'h', b'i']
        let out = info_descr(b"hi");
        assert_eq!(out.len(), 6);
        assert_eq!(out[0], 0xB4, "opcode");
        assert_eq!(out[1], MSG_INFO_DESCR, "message type 22");
        // u16 LE length = 2
        let len = u16::from_le_bytes([out[2], out[3]]);
        assert_eq!(len, 2, "u16 LE length prefix");
        assert_eq!(&out[4..], b"hi", "payload bytes");
    }
}
