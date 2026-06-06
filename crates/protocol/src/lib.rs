#![forbid(unsafe_code)]

//! Wire protocol for Open Tibia 10.98.
//!
//! Contains the [`NetworkMessage`] reader/writer, RSA, XTEA, Adler-32 checksum,
//! and the packet enums for the login (7171) and game (7172) protocols.
//!
//! This crate holds **zero game logic** — only byte layout and crypto.
//!
//! M1 fills in: framing, Adler-32, RSA, XTEA, NetworkMessage, login parse.

pub mod adler;
pub mod challenge;
pub mod charlist;
pub mod frame;
pub mod game_login;
pub mod login;
pub mod message;
pub mod rsa;
pub mod xtea;

pub use adler::adler32;
pub use login::{LoginRequest, parse as parse_login};
pub use message::{MessageReader, MessageWriter};
pub use rsa::RsaPrivateKey;

/// Errors produced while parsing or building protocol messages.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// The buffer ended before a full value could be read.
    #[error("unexpected end of message: needed {needed} bytes, had {had}")]
    UnexpectedEof { needed: usize, had: usize },
}
