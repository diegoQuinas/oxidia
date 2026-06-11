//! Login request packet (opcode `0x01`), protocol 10.98.
//!
//! Layout of the checksum-stripped payload, per `reference/tfs/src/`
//! (`protocollogin.cpp::onRecvFirstMessage` + `protocol.cpp::RSA_decrypt`):
//!
//! ```text
//! u8   opcode (0x01)
//! u16  client OS
//! u16  client version           (>= 971 for the u32 protocol-version field)
//! u32  protocol version
//! u32  .dat signature
//! u32  .spr signature
//! u32  .pic signature
//! u8   0
//! [128]  RSA block, decrypts to:
//!     u8       0                 (padding marker)
//!     u32 x4   XTEA key
//!     string   account name      (u16 length prefix)
//!     string   password          (u16 length prefix)
//!     ...      zero padding
//! ```

use crate::ProtocolError;
use crate::message::{MessageReader, MessageWriter};
use crate::rsa::{RSA_BLOCK_SIZE, RsaError, RsaPrivateKey, encrypt_open_tibia_public};

/// First byte of a login packet.
pub const LOGIN_OPCODE: u8 = 0x01;

/// Below this client version the u32 protocol-version field is absent; we only
/// support modern (10.98) clients, so anything older is rejected.
const MIN_SUPPORTED_VERSION: u16 = 971;

/// A parsed login request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRequest {
    /// Client operating system identifier.
    pub os: u16,
    /// Client protocol version (1098 for the target client).
    pub version: u16,
    /// The 128-bit XTEA session key the client chose, four little-endian words.
    pub xtea_key: [u32; 4],
    /// Account name/number bytes (Latin-1 on the wire).
    pub account: Vec<u8>,
    /// Password bytes (Latin-1 on the wire).
    pub password: Vec<u8>,
}

/// Errors raised while parsing a login packet.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The first byte was not [`LOGIN_OPCODE`].
    #[error("not a login packet: opcode {0:#04x}")]
    UnexpectedOpcode(u8),
    /// The client is too old to speak the modern login layout.
    #[error("client protocol {0} too old (need >= {MIN_SUPPORTED_VERSION})")]
    ProtocolTooOld(u16),
    /// The decrypted RSA block did not begin with the zero padding marker —
    /// usually the wrong RSA key on one side.
    #[error("rsa block leading byte was {0:#04x}, expected 0")]
    RsaPadding(u8),
    /// The buffer ended before a field could be read.
    #[error(transparent)]
    Truncated(#[from] ProtocolError),
    /// The RSA layer rejected the block.
    #[error(transparent)]
    Rsa(#[from] RsaError),
}

/// Build a checksum-stripped login payload as a client would (RSA block
/// encrypted with the public key). Inverse of [`parse`]; used by the replay
/// test and the sniff tooling. Panics if `account`/`password` overflow the RSA
/// block (they never do for real credentials).
pub fn build_request(
    os: u16,
    version: u16,
    xtea_key: [u32; 4],
    account: &[u8],
    password: &[u8],
) -> Vec<u8> {
    // The RSA inner block: padding marker, XTEA key, account, password.
    let mut inner = MessageWriter::new();
    inner.write_u8(0);
    for word in xtea_key {
        inner.write_u32(word);
    }
    inner.write_string(account);
    inner.write_string(password);

    let mut block = [0u8; RSA_BLOCK_SIZE];
    let bytes = inner.into_bytes();
    assert!(
        bytes.len() <= RSA_BLOCK_SIZE,
        "credentials overflow RSA block"
    );
    block[..bytes.len()].copy_from_slice(&bytes);
    encrypt_open_tibia_public(&mut block).expect("128-byte block");

    let mut w = MessageWriter::new();
    w.write_u8(LOGIN_OPCODE);
    w.write_u16(os);
    w.write_u16(version);
    w.write_u32(0); // protocol version
    w.write_u32(0); // .dat signature
    w.write_u32(0); // .spr signature
    w.write_u32(0); // .pic signature
    w.write_u8(0);
    w.write_bytes(&block);
    w.into_bytes()
}

/// Parse a checksum-stripped login payload, decrypting the RSA block with `rsa`.
pub fn parse(payload: &[u8], rsa: &RsaPrivateKey) -> Result<LoginRequest, LoginError> {
    let mut r = MessageReader::new(payload);

    let opcode = r.read_u8()?;
    if opcode != LOGIN_OPCODE {
        return Err(LoginError::UnexpectedOpcode(opcode));
    }

    let os = r.read_u16()?;
    let version = r.read_u16()?;
    if version < MIN_SUPPORTED_VERSION {
        return Err(LoginError::ProtocolTooOld(version));
    }

    let _protocol_version = r.read_u32()?;
    let _dat_signature = r.read_u32()?;
    let _spr_signature = r.read_u32()?;
    let _pic_signature = r.read_u32()?;
    let _reserved = r.read_u8()?;

    let mut block = [0u8; RSA_BLOCK_SIZE];
    block.copy_from_slice(r.read_bytes(RSA_BLOCK_SIZE)?);
    rsa.decrypt(&mut block)?;

    let mut inner = MessageReader::new(&block);
    let pad = inner.read_u8()?;
    if pad != 0 {
        return Err(LoginError::RsaPadding(pad));
    }

    let xtea_key = [
        inner.read_u32()?,
        inner.read_u32()?,
        inner.read_u32()?,
        inner.read_u32()?,
    ];
    let account = inner.read_string()?.to_vec();
    let password = inner.read_string()?.to_vec();

    Ok(LoginRequest {
        os,
        version,
        xtea_key,
        account,
        password,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_round_trips_through_parse() {
        let key = [0x0102_0304, 0x0506_0708, 0x090A_0B0C, 0x0D0E_0F10];
        let payload = build_request(2, 1098, key, b"123456", b"hunter2");

        let req = parse(&payload, &RsaPrivateKey::open_tibia()).unwrap();

        assert_eq!(req.os, 2);
        assert_eq!(req.version, 1098);
        assert_eq!(req.xtea_key, key);
        assert_eq!(req.account, b"123456");
        assert_eq!(req.password, b"hunter2");
    }

    #[test]
    fn rejects_a_non_login_opcode() {
        let mut payload = build_request(2, 1098, [0; 4], b"a", b"b");
        payload[0] = 0x0F;
        match parse(&payload, &RsaPrivateKey::open_tibia()) {
            Err(LoginError::UnexpectedOpcode(0x0F)) => {}
            other => panic!("expected UnexpectedOpcode, got {other:?}"),
        }
    }

    #[test]
    fn rejects_an_ancient_client_version() {
        let payload = build_request(2, 770, [0; 4], b"a", b"b");
        match parse(&payload, &RsaPrivateKey::open_tibia()) {
            Err(LoginError::ProtocolTooOld(770)) => {}
            other => panic!("expected ProtocolTooOld, got {other:?}"),
        }
    }
}
