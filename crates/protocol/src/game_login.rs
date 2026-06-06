//! Parser for the first client->game packet (protocol 10.98).
//! Mirrors `reference/tfs/src/protocolgame.cpp` (`onRecvFirstMessage`).
//! Layout: `[u8 0x0A][u16 os][u16 version][7 skipped bytes][128-byte RSA block]`.
//! RSA block: `[u8 0][u32x4 xtea][u8 gamemaster][string sessionKey][string name][u32 ts][u8 rnd]`.

use crate::message::{MessageReader, MessageWriter};
use crate::rsa::{self, RsaError, RsaPrivateKey, RSA_BLOCK_SIZE};
use crate::ProtocolError;

/// ProtocolGame identifier byte (TFS `ProtocolGame::protocolIdentifier` = 0x0A).
pub const GAME_PROTOCOL_ID: u8 = 0x0A;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameLoginRequest {
    pub os: u16,
    pub version: u16,
    pub xtea_key: [u32; 4],
    /// Untrusted client-reported flag. The server must NEVER grant privileges
    /// based on this value alone — TFS deliberately ignores it.
    pub gamemaster: bool,
    pub account: Vec<u8>,
    pub password: Vec<u8>,
    pub character_name: Vec<u8>,
    pub challenge_timestamp: u32,
    pub challenge_random: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum GameLoginError {
    #[error("unexpected protocol id byte {0:#04x}")]
    UnexpectedProtocolId(u8),
    #[error("rsa padding byte was {0}, expected 0")]
    RsaPadding(u8),
    #[error("session key had {0} parts, expected 4 (account\\npassword\\ntoken\\ntokenTime)")]
    MalformedSessionKey(usize),
    #[error(transparent)]
    Truncated(#[from] ProtocolError),
    #[error(transparent)]
    Rsa(#[from] RsaError),
}

/// Parse a checksum-stripped game-login payload.
pub fn parse(payload: &[u8], rsa: &RsaPrivateKey) -> Result<GameLoginRequest, GameLoginError> {
    let mut r = MessageReader::new(payload);

    let id = r.read_u8()?;
    if id != GAME_PROTOCOL_ID {
        return Err(GameLoginError::UnexpectedProtocolId(id));
    }
    let os = r.read_u16()?;
    let version = r.read_u16()?;
    let _ = r.read_bytes(7)?; // u32 clientVersion + u8 clientType + u16 datRevision

    let mut block = [0u8; RSA_BLOCK_SIZE];
    block.copy_from_slice(r.read_bytes(RSA_BLOCK_SIZE)?);
    rsa.decrypt(&mut block)?;

    let mut inner = MessageReader::new(&block);
    let pad = inner.read_u8()?;
    if pad != 0 {
        return Err(GameLoginError::RsaPadding(pad));
    }
    let xtea_key = [
        inner.read_u32()?,
        inner.read_u32()?,
        inner.read_u32()?,
        inner.read_u32()?,
    ];
    let gamemaster = inner.read_u8()? != 0;
    let session_key = inner.read_string()?.to_vec();
    let character_name = inner.read_string()?.to_vec();
    let challenge_timestamp = inner.read_u32()?;
    let challenge_random = inner.read_u8()?;

    let (account, password) = split_session_key(&session_key)?;

    Ok(GameLoginRequest {
        os,
        version,
        xtea_key,
        gamemaster,
        account,
        password,
        character_name,
        challenge_timestamp,
        challenge_random,
    })
}

/// `sessionKey` is `account\npassword\ntoken\ntokenTime` (TFS uses exactly 4 parts).
fn split_session_key(session_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>), GameLoginError> {
    let parts: Vec<&[u8]> = session_key.split(|&b| b == b'\n').collect();
    if parts.len() != 4 {
        return Err(GameLoginError::MalformedSessionKey(parts.len()));
    }
    Ok((parts[0].to_vec(), parts[1].to_vec()))
}

/// Build a client-side game-login payload (RSA-public-encrypted) for tests/tooling.
#[allow(clippy::too_many_arguments)]
pub fn build_request(
    os: u16,
    version: u16,
    xtea_key: [u32; 4],
    account: &[u8],
    password: &[u8],
    character_name: &[u8],
    challenge_timestamp: u32,
    challenge_random: u8,
) -> Result<Vec<u8>, RsaError> {
    let mut w = MessageWriter::new();
    w.write_u8(GAME_PROTOCOL_ID);
    w.write_u16(os);
    w.write_u16(version);
    w.write_bytes(&[0u8; 7]); // clientVersion + clientType + datRevision

    let mut block = vec![0u8; RSA_BLOCK_SIZE];
    {
        let mut inner = MessageWriter::new();
        inner.write_u8(0); // padding sentinel
        for k in xtea_key {
            inner.write_u32(k);
        }
        inner.write_u8(0); // gamemaster
        let mut session = Vec::new();
        session.extend_from_slice(account);
        session.push(b'\n');
        session.extend_from_slice(password);
        session.extend_from_slice(b"\n\n0"); // empty token + tokenTime
        inner.write_string(&session);
        inner.write_string(character_name);
        inner.write_u32(challenge_timestamp);
        inner.write_u8(challenge_random);
        let bytes = inner.into_bytes();
        assert!(bytes.len() <= RSA_BLOCK_SIZE, "rsa inner block overflow");
        block[..bytes.len()].copy_from_slice(&bytes);
    }
    rsa::encrypt_open_tibia_public(&mut block)?;
    w.write_bytes(&block);
    Ok(w.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_built_request() {
        let key = [0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444];
        let payload = build_request(
            10, 1098, key, b"test", b"test", b"Test Knight", 0xDEAD_BEEF, 0x7C,
        )
        .unwrap();

        let rsa = RsaPrivateKey::open_tibia();
        let req = parse(&payload, &rsa).unwrap();

        assert_eq!(req.os, 10);
        assert_eq!(req.version, 1098);
        assert_eq!(req.xtea_key, key);
        assert!(!req.gamemaster);
        assert_eq!(req.account, b"test");
        assert_eq!(req.password, b"test");
        assert_eq!(req.character_name, b"Test Knight");
        assert_eq!(req.challenge_timestamp, 0xDEAD_BEEF);
        assert_eq!(req.challenge_random, 0x7C);
    }

    #[test]
    fn rejects_wrong_protocol_id() {
        let rsa = RsaPrivateKey::open_tibia();
        let err = parse(&[0x01, 0, 0], &rsa).unwrap_err();
        assert!(matches!(err, GameLoginError::UnexpectedProtocolId(0x01)));
    }

    #[test]
    fn rejects_malformed_session_key() {
        assert!(matches!(
            split_session_key(b"no-newlines-here"),
            Err(GameLoginError::MalformedSessionKey(1))
        ));
        assert!(split_session_key(b"a\nb\nc\nd").is_ok());
    }

    #[test]
    fn rejects_nonzero_rsa_padding() {
        let mut w = MessageWriter::new();
        w.write_u8(GAME_PROTOCOL_ID);
        w.write_u16(10);
        w.write_u16(1098);
        w.write_bytes(&[0u8; 7]);
        let mut block = vec![0u8; RSA_BLOCK_SIZE];
        block[0] = 1; // non-zero padding sentinel
        rsa::encrypt_open_tibia_public(&mut block).unwrap();
        w.write_bytes(&block);
        let payload = w.into_bytes();

        let rsa_key = RsaPrivateKey::open_tibia();
        let err = parse(&payload, &rsa_key).unwrap_err();
        assert!(matches!(err, GameLoginError::RsaPadding(1)));
    }
}
