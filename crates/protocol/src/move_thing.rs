//! Inbound `0x78` move-thing (TFS `parseThrow`, `protocolgame.cpp:509`). Body:
//! `[from x u16,y u16,z u8][spriteId u16][from_stackpos u8][to x u16,y u16,z u8][count u8]`.
//! Inventory/container endpoints use `x == 0xFFFF` — M10.1 forwards them as-is and
//! the world rejects non-map positions (those land in M10.2 / M10.3).

use crate::message::MessageReader;

/// Parsed move-thing request. Positions are raw wire coords (`x == 0xFFFF` flags
/// an inventory/container slot — out of scope for M10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throw {
    pub from: (u16, u16, u8),
    pub sprite: u16,
    pub from_stackpos: u8,
    pub to: (u16, u16, u8),
    pub count: u8,
}

/// Parse the `0x78` body (everything after the opcode byte). `None` if malformed.
pub fn parse_throw(body: &[u8]) -> Option<Throw> {
    let mut r = MessageReader::new(body);
    let fx = r.read_u16().ok()?;
    let fy = r.read_u16().ok()?;
    let fz = r.read_u8().ok()?;
    let sprite = r.read_u16().ok()?;
    let from_stackpos = r.read_u8().ok()?;
    let tx = r.read_u16().ok()?;
    let ty = r.read_u16().ok()?;
    let tz = r.read_u8().ok()?;
    let count = r.read_u8().ok()?;
    Some(Throw { from: (fx, fy, fz), sprite, from_stackpos, to: (tx, ty, tz), count })
}
