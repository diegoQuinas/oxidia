//! Generic OTB node-tree container.
//!
//! Both `items.otb` and the `.otbm` map are stored as the same recursive node
//! tree (see TFS `fileloader.cpp`). Layout:
//!
//! ```text
//! [u8;4 identifier][0xFE root-type][root props...]( child | 0xFF )*
//! ```
//!
//! - `0xFE` (START) opens a child node; the next byte is the child's type.
//! - `0xFF` (END) closes the current node.
//! - `0xFD` (ESCAPE) makes the following byte literal prop data, so a
//!   `0xFE`/`0xFF`/`0xFD` byte can appear inside props.
//!
//! `props` are returned already un-escaped.

use crate::FormatError;

/// A node start marker.
const START: u8 = 0xFE;
/// A node end marker.
const END: u8 = 0xFF;
/// Escapes the following byte so it is treated as literal prop data.
const ESCAPE: u8 = 0xFD;

/// One node in an OTB tree: a type byte, its (un-escaped) property bytes, and
/// its child nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The node's type byte (meaning is format-specific).
    pub kind: u8,
    /// Property bytes, with escape bytes already removed.
    pub props: Vec<u8>,
    /// Child nodes, in file order.
    pub children: Vec<Node>,
}

/// Parse a full OTB file into its root node.
///
/// The first 4 bytes are the format identifier and are not interpreted here.
pub fn parse_tree(data: &[u8]) -> Result<Node, FormatError> {
    // [u8;4 identifier][0xFE][root-type] ...
    if data.len() < 6 {
        return Err(FormatError::UnexpectedEof {
            what: "node header",
        });
    }
    if data[4] != START {
        return Err(FormatError::InvalidNode {
            what: "missing root start marker",
        });
    }
    let kind = data[5];
    let mut pos = 6;
    let node = parse_node(data, &mut pos, kind)?;
    Ok(node)
}

/// Parse one node's props and children. `pos` points just past the node's type
/// byte on entry and just past the node's `END` marker on return.
fn parse_node(data: &[u8], pos: &mut usize, kind: u8) -> Result<Node, FormatError> {
    let mut props = Vec::new();
    let mut children = Vec::new();
    loop {
        let byte = *data
            .get(*pos)
            .ok_or(FormatError::UnexpectedEof { what: "node body" })?;
        match byte {
            START => {
                *pos += 1;
                let child_kind = *data
                    .get(*pos)
                    .ok_or(FormatError::UnexpectedEof { what: "child type" })?;
                *pos += 1;
                children.push(parse_node(data, pos, child_kind)?);
            }
            END => {
                *pos += 1;
                return Ok(Node {
                    kind,
                    props,
                    children,
                });
            }
            ESCAPE => {
                *pos += 1;
                let literal = *data.get(*pos).ok_or(FormatError::UnexpectedEof {
                    what: "escaped byte",
                })?;
                props.push(literal);
                *pos += 1;
            }
            other => {
                props.push(other);
                *pos += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_root_with_props_and_no_children() {
        // [identifier][FE][type=5][props AA BB][FF]
        let data = [0x00, 0x00, 0x00, 0x00, START, 0x05, 0xAA, 0xBB, END];
        let root = parse_tree(&data).unwrap();
        assert_eq!(root.kind, 0x05);
        assert_eq!(root.props, vec![0xAA, 0xBB]);
        assert!(root.children.is_empty());
    }

    #[test]
    fn parses_nested_children_in_order() {
        // root(type 0, props 01) { childA(type 5, props AA) childB(type 6, props BB) }
        let data = [
            0x00, 0x00, 0x00, 0x00, // identifier
            START, 0x00, 0x01, // root type 0, prop 01
            START, 0x05, 0xAA, END, // child A
            START, 0x06, 0xBB, END, // child B
            END, // close root
        ];
        let root = parse_tree(&data).unwrap();
        assert_eq!(root.kind, 0x00);
        assert_eq!(root.props, vec![0x01]);
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].kind, 0x05);
        assert_eq!(root.children[0].props, vec![0xAA]);
        assert_eq!(root.children[1].kind, 0x06);
        assert_eq!(root.children[1].props, vec![0xBB]);
    }

    #[test]
    fn unescapes_marker_bytes_in_props() {
        // props contain escaped 0xFE, 0xFF, 0xFD via the 0xFD escape byte.
        let data = [
            0x00, 0x00, 0x00, 0x00, START, 0x05, //
            ESCAPE, START, ESCAPE, END, ESCAPE, ESCAPE, 0x42, END,
        ];
        let root = parse_tree(&data).unwrap();
        assert_eq!(root.props, vec![START, END, ESCAPE, 0x42]);
        assert!(root.children.is_empty());
    }

    /// Path to a real reference file, or `None` if the (gitignored) TFS data
    /// tree is not present in this checkout.
    fn reference_file(rel: &str) -> Option<std::path::PathBuf> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/tfs/data")
            .join(rel);
        path.exists().then_some(path)
    }

    #[test]
    fn parses_the_real_forgotten_otbm_root() {
        let Some(path) = reference_file("world/forgotten.otbm") else {
            eprintln!("skipping: reference/tfs not present");
            return;
        };
        let data = std::fs::read(path).unwrap();
        let root = parse_tree(&data).unwrap();
        // TFS never validates the root type byte; it reads the version from the
        // OTBM_root_header in the root props and checks the single child is
        // OTBM_MAP_DATA = 2. The root's props hold the 16-byte header.
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].kind, 2);
        assert!(root.props.len() >= 16, "root props hold the OTBM header");
    }

    #[test]
    fn rejects_truncated_input_before_end() {
        // root opened but never closed.
        let data = [0x00, 0x00, 0x00, 0x00, START, 0x05, 0xAA];
        match parse_tree(&data) {
            Err(FormatError::UnexpectedEof { .. }) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }
}
