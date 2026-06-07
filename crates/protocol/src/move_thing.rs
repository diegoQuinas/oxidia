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

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire layout (14 bytes):
    /// [fx u16 LE][fy u16 LE][fz u8][sprite u16 LE][from_stackpos u8]
    /// [tx u16 LE][ty u16 LE][tz u8][count u8]
    #[test]
    fn parse_throw_round_trips_full_body() {
        let mut body = Vec::new();
        body.extend_from_slice(&1000u16.to_le_bytes()); // fx
        body.extend_from_slice(&2000u16.to_le_bytes()); // fy
        body.push(7u8);                                  // fz
        body.extend_from_slice(&4526u16.to_le_bytes()); // sprite
        body.push(2u8);                                  // from_stackpos
        body.extend_from_slice(&1001u16.to_le_bytes()); // tx
        body.extend_from_slice(&2000u16.to_le_bytes()); // ty
        body.push(7u8);                                  // tz
        body.push(5u8);                                  // count
        assert_eq!(body.len(), 14, "body must be exactly 14 bytes");
        let t = parse_throw(&body).expect("valid 14-byte body must parse");
        assert_eq!(t.from, (1000, 2000, 7));
        assert_eq!(t.sprite, 4526);
        assert_eq!(t.from_stackpos, 2);
        assert_eq!(t.to, (1001, 2000, 7));
        assert_eq!(t.count, 5);
    }

    #[test]
    fn parse_throw_returns_none_on_short_body() {
        // 13 bytes (one short) must return None, not panic.
        let body = [0u8; 13];
        assert!(parse_throw(&body).is_none(), "13-byte body must not parse");
    }

    #[test]
    fn parse_throw_returns_none_on_empty_body() {
        assert!(parse_throw(&[]).is_none());
    }
}
