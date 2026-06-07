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
