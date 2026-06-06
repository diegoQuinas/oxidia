//! Login server responses: the character list and login errors.
//!
//! Byte layout mirrors `reference/tfs/src/protocollogin.cpp`
//! (`getCharacterList` and `disconnectClient`) for the single-world, no-2FA
//! case. These functions build the **payload** only; the `net` layer adds the
//! Adler-32 checksum and the `u16` length prefix before sending.

use crate::message::MessageWriter;

/// Opcode for a Message-Of-The-Day block.
pub const OPCODE_MOTD: u8 = 0x14;
/// Opcode for the session key block.
pub const OPCODE_SESSION_KEY: u8 = 0x28;
/// Opcode for the character list block.
pub const OPCODE_CHARACTER_LIST: u8 = 0x64;
/// Error opcode for clients older than 1076.
pub const OPCODE_ERROR_OLD: u8 = 0x0A;
/// Error opcode for clients 1076 and newer.
pub const OPCODE_ERROR: u8 = 0x0B;

/// The single game world advertised to the client.
#[derive(Debug, Clone)]
pub struct World<'a> {
    /// World name shown in the client.
    pub name: &'a str,
    /// Host/IP the client connects to for the game protocol.
    pub host: &'a str,
    /// Game protocol port.
    pub port: u16,
}

/// Everything needed to build a successful character-list response.
#[derive(Debug, Clone)]
pub struct CharacterList<'a> {
    /// Optional Message Of The Day as `(number, text)`.
    pub motd: Option<(u32, &'a str)>,
    /// Session key string the client echoes to the game server.
    pub session_key: &'a str,
    /// The advertised game world.
    pub world: World<'a>,
    /// Character names on the account.
    pub characters: &'a [String],
    /// Unix timestamp the account's premium ends (0 = none).
    pub premium_ends_at: u32,
}

impl CharacterList<'_> {
    /// Encode the full character-list payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = MessageWriter::new();

        if let Some((num, text)) = self.motd {
            w.write_u8(OPCODE_MOTD);
            w.write_string(format!("{num}\n{text}").as_bytes());
        }

        w.write_u8(OPCODE_SESSION_KEY);
        w.write_string(self.session_key.as_bytes());

        w.write_u8(OPCODE_CHARACTER_LIST);
        // One world.
        w.write_u8(1);
        w.write_u8(0); // world id
        w.write_string(self.world.name.as_bytes());
        w.write_string(self.world.host.as_bytes());
        w.write_u16(self.world.port);
        w.write_u8(0); // preview state

        let count = self.characters.len().min(u8::MAX as usize) as u8;
        w.write_u8(count);
        for character in self.characters.iter().take(count as usize) {
            w.write_u8(0); // status (0 = offline; M1 always offline)
            w.write_string(character.as_bytes());
        }

        // Premium trailer.
        w.write_u8(0);
        w.write_u8(if self.premium_ends_at > 0 { 1 } else { 0 });
        w.write_u32(self.premium_ends_at);

        w.into_bytes()
    }
}

/// Build a login error payload. The opcode depends on the client `version`
/// (TFS uses `0x0B` for >= 1076, otherwise `0x0A`).
pub fn build_error(message: &str, version: u16) -> Vec<u8> {
    let mut w = MessageWriter::new();
    w.write_u8(if version >= 1076 { OPCODE_ERROR } else { OPCODE_ERROR_OLD });
    w.write_string(message.as_bytes());
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MessageReader;

    fn sample() -> CharacterList<'static> {
        CharacterList {
            motd: Some((42, "Welcome!")),
            session_key: "test\ntest\n\n0",
            world: World { name: "Tibia", host: "127.0.0.1", port: 7172 },
            characters: &[],
            premium_ends_at: 0,
        }
    }

    #[test]
    fn encodes_motd_session_world_and_characters() {
        let characters = vec!["Test Knight".to_string(), "Test Sorcerer".to_string()];
        let list = CharacterList { characters: &characters, ..sample() };

        let bytes = list.encode();
        let mut r = MessageReader::new(&bytes);

        assert_eq!(r.read_u8().unwrap(), OPCODE_MOTD);
        assert_eq!(r.read_string().unwrap(), b"42\nWelcome!");

        assert_eq!(r.read_u8().unwrap(), OPCODE_SESSION_KEY);
        assert_eq!(r.read_string().unwrap(), b"test\ntest\n\n0");

        assert_eq!(r.read_u8().unwrap(), OPCODE_CHARACTER_LIST);
        assert_eq!(r.read_u8().unwrap(), 1, "world count");
        assert_eq!(r.read_u8().unwrap(), 0, "world id");
        assert_eq!(r.read_string().unwrap(), b"Tibia");
        assert_eq!(r.read_string().unwrap(), b"127.0.0.1");
        assert_eq!(r.read_u16().unwrap(), 7172);
        assert_eq!(r.read_u8().unwrap(), 0, "world preview state");

        assert_eq!(r.read_u8().unwrap(), 2, "character count");
        assert_eq!(r.read_u8().unwrap(), 0, "status byte");
        assert_eq!(r.read_string().unwrap(), b"Test Knight");
        assert_eq!(r.read_u8().unwrap(), 0, "status byte");
        assert_eq!(r.read_string().unwrap(), b"Test Sorcerer");

        assert_eq!(r.read_u8().unwrap(), 0, "premium reserved byte");
        assert_eq!(r.read_u8().unwrap(), 0, "premium flag");
        assert_eq!(r.read_u32().unwrap(), 0, "premium ends at");
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn omits_motd_block_when_absent() {
        let list = CharacterList { motd: None, ..sample() };
        let bytes = list.encode();
        // First opcode must be the session key, not the MOTD.
        assert_eq!(bytes[0], OPCODE_SESSION_KEY);
    }

    #[test]
    fn error_uses_modern_opcode_for_recent_clients() {
        let bytes = build_error("nope", 1098);
        let mut r = MessageReader::new(&bytes);
        assert_eq!(r.read_u8().unwrap(), OPCODE_ERROR);
        assert_eq!(r.read_string().unwrap(), b"nope");
    }

    #[test]
    fn error_uses_legacy_opcode_for_old_clients() {
        let bytes = build_error("nope", 1000);
        assert_eq!(bytes[0], OPCODE_ERROR_OLD);
    }
}
