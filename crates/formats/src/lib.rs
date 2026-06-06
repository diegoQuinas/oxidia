#![forbid(unsafe_code)]

//! Parsers for Open Tibia file formats.
//!
//! `.otb` (items) and `.otbm` (map). Designed as pure libraries: they parse
//! from `&[u8]` / `impl Read` and make no assumptions about where the bytes
//! came from. The server never touches `.spr`/`.dat` — only `items.otb` and
//! the `.otbm` map.
//!
//! M2 fills in the actual parsers.

pub mod node;
pub mod otb;
pub mod otbm;
pub mod props;

/// Errors produced while parsing an OT file format.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    /// The input ended before a full record could be read.
    #[error("unexpected end of input while parsing {what}")]
    UnexpectedEof { what: &'static str },
    /// The node tree was structurally invalid.
    #[error("invalid node structure: {what}")]
    InvalidNode { what: &'static str },
}
