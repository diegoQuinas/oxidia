//! Chat packets for protocol 10.98: inbound `0x96` "say" (parse) and outbound
//! `0xAA` "creature say" (build). Byte-faithful ports of
//! `reference/tfs/src/protocolgame.cpp` `parseSay` (922-951) and
//! `sendCreatureSay` (2199-2225). M6 supports the three local speak types
//! (say/whisper/yell); private and channel types are rejected by `parse_say`.

use crate::message::{MessageReader, MessageWriter};

pub const OP_CREATURE_SAY: u8 = 0xAA;

/// The local speak types M6 supports. Wire values match TFS `SpeakClasses`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakType {
    Say = 1,
    Whisper = 2,
    Yell = 3,
}

impl SpeakType {
    /// Map a wire byte to a supported local speak type (`None` otherwise).
    pub fn from_u8(b: u8) -> Option<SpeakType> {
        match b {
            1 => Some(SpeakType::Say),
            2 => Some(SpeakType::Whisper),
            3 => Some(SpeakType::Yell),
            _ => None,
        }
    }

    /// The wire byte for this speak type.
    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Parse the body of an inbound `0x96` (the bytes AFTER the opcode). Returns the
/// speak type + message for say/whisper/yell. Returns `None` for an unsupported
/// speak type (private/channel), a malformed/short body, or an empty message.
pub fn parse_say(body: &[u8]) -> Option<(SpeakType, String)> {
    let mut r = MessageReader::new(body);
    let speak_type = SpeakType::from_u8(r.read_u8().ok()?)?;
    let text = r.read_string().ok()?;
    if text.is_empty() {
        return None;
    }
    Some((speak_type, String::from_utf8_lossy(text).into_owned()))
}

/// Build a `0xAA` creature-say (position form):
/// `[0xAA][stmt u32][name str][level u16][type u8][x u16][y u16][z u8][msg str]`.
pub fn creature_say(
    statement_id: u32,
    name: &[u8],
    level: u16,
    speak_type: SpeakType,
    pos: (u16, u16, u8),
    text: &[u8],
) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(OP_CREATURE_SAY);
    w.write_u32(statement_id);
    w.write_string(name);
    w.write_u16(level);
    w.write_u8(speak_type.to_u8());
    w.write_u16(pos.0);
    w.write_u16(pos.1);
    w.write_u8(pos.2);
    w.write_string(text);
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn say_body(speak_type: u8, msg: &[u8]) -> Vec<u8> {
        let mut b = vec![speak_type];
        b.extend_from_slice(&(msg.len() as u16).to_le_bytes());
        b.extend_from_slice(msg);
        b
    }

    #[test]
    fn parse_say_reads_type_and_message() {
        let (t, msg) = parse_say(&say_body(1, b"hello")).unwrap();
        assert_eq!(t, SpeakType::Say);
        assert_eq!(msg, "hello");
    }

    #[test]
    fn parse_say_accepts_whisper_and_yell() {
        assert_eq!(parse_say(&say_body(2, b"hi")).unwrap().0, SpeakType::Whisper);
        assert_eq!(parse_say(&say_body(3, b"hi")).unwrap().0, SpeakType::Yell);
    }

    #[test]
    fn parse_say_rejects_unsupported_types() {
        for b in [0u8, 5, 7, 36] {
            assert!(parse_say(&say_body(b, b"hi")).is_none(), "type {b} must be rejected");
        }
    }

    #[test]
    fn parse_say_rejects_empty_and_truncated() {
        assert!(parse_say(&say_body(1, b"")).is_none(), "empty message");
        assert!(parse_say(&[]).is_none(), "no body");
        let mut t = vec![1u8];
        t.extend_from_slice(&5u16.to_le_bytes());
        t.push(b'x');
        assert!(parse_say(&t).is_none(), "truncated string");
    }

    #[test]
    fn creature_say_layout() {
        let p = creature_say(0x0A0B_0C0D, b"Bob", 1, SpeakType::Say, (100, 200, 7), b"hi");
        let mut i = 0;
        assert_eq!(p[i], OP_CREATURE_SAY); i += 1;
        assert_eq!(u32::from_le_bytes([p[i], p[i+1], p[i+2], p[i+3]]), 0x0A0B_0C0D); i += 4;
        assert_eq!(u16::from_le_bytes([p[i], p[i+1]]), 3); i += 2; // name len
        assert_eq!(&p[i..i+3], b"Bob"); i += 3;
        assert_eq!(u16::from_le_bytes([p[i], p[i+1]]), 1); i += 2; // level
        assert_eq!(p[i], 1); i += 1; // type SAY
        assert_eq!(u16::from_le_bytes([p[i], p[i+1]]), 100); i += 2; // x
        assert_eq!(u16::from_le_bytes([p[i], p[i+1]]), 200); i += 2; // y
        assert_eq!(p[i], 7); i += 1; // z
        assert_eq!(u16::from_le_bytes([p[i], p[i+1]]), 2); i += 2; // msg len
        assert_eq!(&p[i..i+2], b"hi"); i += 2;
        assert_eq!(i, p.len(), "no trailing bytes");
    }
}
