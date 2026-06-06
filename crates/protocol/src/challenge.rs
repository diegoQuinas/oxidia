//! The game server's first packet: the `0x1F` login challenge.
//! Checksummed (Adler-32) but NOT XTEA-encrypted — XTEA is enabled only after
//! the client's first packet is parsed. See `reference/tfs/src/protocolgame.cpp`
//! (`ProtocolGame::onConnect`).

use crate::message::MessageWriter;

pub const OPCODE_CHALLENGE: u8 = 0x1F;

/// Encode the challenge payload: `[0x1F][u32 LE timestamp][u8 random]`.
pub fn encode(timestamp: u32, random: u8) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OPCODE_CHALLENGE);
    w.write_u32(timestamp);
    w.write_u8(random);
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_opcode_timestamp_and_random() {
        let bytes = encode(0x1122_3344, 0xAB);
        assert_eq!(bytes, [0x1F, 0x44, 0x33, 0x22, 0x11, 0xAB]);
    }
}
