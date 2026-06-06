//! Parser for `items.otb` (the server item dictionary).
//!
//! The file is an OTB node tree (see [`crate::node`]). The root props carry a
//! [`VERSIONINFO`]-style version block; each child node is one item, whose node
//! type is its `itemgroup_t` and whose props are `[u32 flags]` followed by
//! `[u8 attr][u16 len][data]` records.
//!
//! Ref: TFS `items.cpp::loadFromOtb` and `itemloader.h`.

use crate::node::Node;
use crate::props::PropReader;
use crate::FormatError;

/// Root attribute: version block.
const ROOT_ATTR_VERSION: u8 = 0x01;

/// Item attribute opcodes we interpret (see `itemattrib_t`).
const ITEM_ATTR_SERVER_ID: u8 = 0x10;
const ITEM_ATTR_CLIENT_ID: u8 = 0x11;
/// `ITEM_ATTR_TOPORDER` — a single byte: the always-on-top draw order.
const ITEM_ATTR_TOP_ORDER: u8 = 0x2B;

/// `FLAG_ALWAYSONTOP` bit of the per-item flags word (`itemflags_t`).
const FLAG_ALWAYS_ON_TOP: u32 = 1 << 13;
/// `FLAG_STACKABLE` — the wire carries a u8 count after the item's mark byte.
const FLAG_STACKABLE: u32 = 1 << 7;
/// `FLAG_ANIMATION` — the wire carries a u8 animation-phase byte after the item.
const FLAG_ANIMATION: u32 = 1 << 24;

/// `ITEM_GROUP_GROUND` (`itemgroup_t`) — a ground-tile item.
const ITEM_GROUP_GROUND: u8 = 1;
/// `ITEM_GROUP_SPLASH` / `ITEM_GROUP_FLUID` — carry a u8 fluid-type byte on the wire.
const ITEM_GROUP_SPLASH: u8 = 11;
const ITEM_GROUP_FLUID: u8 = 12;

/// A parsed `items.otb`: version metadata plus the item table in file order.
#[derive(Debug, Clone)]
pub struct ItemsOtb {
    /// OTB format version (3 for modern files).
    pub major_version: u32,
    /// Encodes the client version (57 == client 10.98).
    pub minor_version: u32,
    /// Build/revision number.
    pub build_number: u32,
    /// One entry per item node, in file order.
    pub items: Vec<ItemType>,
}

/// A single item definition from `items.otb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemType {
    /// `itemgroup_t` (the node type byte).
    pub group: u8,
    /// Item behaviour flags (`itemflags_t` bitset).
    pub flags: u32,
    /// Server-side item id.
    pub server_id: u16,
    /// Client-side (sprite) item id.
    pub client_id: u16,
    /// `FLAG_ALWAYSONTOP` — renders above creatures; ordered by `top_order`.
    pub always_on_top: bool,
    /// `ITEM_ATTR_TOPORDER` — draw order among always-on-top items (0 if absent).
    pub top_order: u8,
}

impl ItemType {
    /// True if this item is a ground tile (`itemgroup_t == ITEM_GROUP_GROUND`).
    pub fn is_ground(&self) -> bool {
        self.group == ITEM_GROUP_GROUND
    }

    /// `FLAG_STACKABLE` — the wire form carries a u8 count byte for this item.
    pub fn is_stackable(&self) -> bool {
        self.flags & FLAG_STACKABLE != 0
    }

    /// `FLAG_ANIMATION` — the wire form carries a u8 animation-phase byte.
    pub fn is_animated(&self) -> bool {
        self.flags & FLAG_ANIMATION != 0
    }

    /// Splash or fluid container — the wire form carries a u8 fluid-type byte.
    pub fn is_fluid_or_splash(&self) -> bool {
        self.group == ITEM_GROUP_SPLASH || self.group == ITEM_GROUP_FLUID
    }
}

/// Parse a full `items.otb` byte buffer.
pub fn parse(data: &[u8]) -> Result<ItemsOtb, FormatError> {
    let root = crate::node::parse_tree(data)?;
    let (major_version, minor_version, build_number) = parse_version(&root.props)?;

    let mut items = Vec::with_capacity(root.children.len());
    for item_node in &root.children {
        items.push(parse_item(item_node)?);
    }

    Ok(ItemsOtb { major_version, minor_version, build_number, items })
}

/// Read the root version block: `[u32 flags][u8 ROOT_ATTR_VERSION][u16 len][major][minor][build]`.
fn parse_version(props: &[u8]) -> Result<(u32, u32, u32), FormatError> {
    let mut r = PropReader::new(props);
    let _flags = r.read_u32()?;
    let attr = r.read_u8()?;
    if attr != ROOT_ATTR_VERSION {
        return Err(FormatError::InvalidNode { what: "expected items.otb version attribute" });
    }
    let _datalen = r.read_u16()?;
    let major = r.read_u32()?;
    let minor = r.read_u32()?;
    let build = r.read_u32()?;
    Ok((major, minor, build))
}

/// Read one item node: `[u32 flags]` then `[u8 attr][u16 len][data]` records.
fn parse_item(node: &Node) -> Result<ItemType, FormatError> {
    let mut r = PropReader::new(&node.props);
    let flags = r.read_u32()?;
    let mut server_id = 0;
    let mut client_id = 0;
    let mut top_order = 0u8;
    while r.remaining() > 0 {
        let attr = r.read_u8()?;
        let len = r.read_u16()? as usize;
        match attr {
            ITEM_ATTR_SERVER_ID => server_id = r.read_u16()?,
            ITEM_ATTR_CLIENT_ID => client_id = r.read_u16()?,
            ITEM_ATTR_TOP_ORDER => top_order = r.read_u8()?,
            _ => r.skip(len)?,
        }
    }
    Ok(ItemType {
        group: node.kind,
        flags,
        server_id,
        client_id,
        always_on_top: flags & FLAG_ALWAYS_ON_TOP != 0,
        top_order,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but well-formed `items.otb` with one ground item.
    fn synthetic_otb() -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x00]; // identifier
        v.push(0xFE); // root START
        v.push(0x00); // root type
        v.extend_from_slice(&0u32.to_le_bytes()); // root flags
        v.push(ROOT_ATTR_VERSION);
        v.extend_from_slice(&140u16.to_le_bytes()); // sizeof VERSIONINFO
        v.extend_from_slice(&3u32.to_le_bytes()); // major
        v.extend_from_slice(&57u32.to_le_bytes()); // minor
        v.extend_from_slice(&0u32.to_le_bytes()); // build
        v.extend_from_slice(&[0u8; 128]); // CSDVersion

        v.push(0xFE); // item START
        v.push(0x01); // group = ITEM_GROUP_GROUND
        v.extend_from_slice(&0x80u32.to_le_bytes()); // flags (FLAG_STACKABLE)
        v.push(ITEM_ATTR_SERVER_ID);
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&100u16.to_le_bytes());
        v.push(ITEM_ATTR_CLIENT_ID);
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&4526u16.to_le_bytes());
        v.push(0xFF); // item END

        v.push(0xFF); // root END
        v
    }

    fn reference_items_otb() -> Option<Vec<u8>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/tfs/data/items/items.otb");
        path.exists().then(|| std::fs::read(path).unwrap())
    }

    #[test]
    fn parses_the_real_items_otb() {
        let Some(data) = reference_items_otb() else {
            eprintln!("skipping: reference/tfs not present");
            return;
        };
        let otb = parse(&data).unwrap();
        // Header advertises "OTB 3.57.62-10.98": major 3, minor 57 (client 1098).
        assert_eq!(otb.major_version, 3);
        assert_eq!(otb.minor_version, 57);
        // A full item dictionary has thousands of entries.
        assert!(otb.items.len() > 5000, "got {} items", otb.items.len());
        // Every item carries a server id, and most carry a client id.
        assert!(otb.items.iter().all(|i| i.server_id != 0));
    }

    /// An always-on-top item with a TOPORDER attribute parses both fields.
    #[test]
    fn parses_always_on_top_and_top_order() {
        let mut v = vec![0x00, 0x00, 0x00, 0x00]; // identifier
        v.push(0xFE); // root START
        v.push(0x00); // root type
        v.extend_from_slice(&0u32.to_le_bytes()); // root flags
        v.push(ROOT_ATTR_VERSION);
        v.extend_from_slice(&140u16.to_le_bytes());
        v.extend_from_slice(&3u32.to_le_bytes()); // major
        v.extend_from_slice(&57u32.to_le_bytes()); // minor
        v.extend_from_slice(&0u32.to_le_bytes()); // build
        v.extend_from_slice(&[0u8; 128]); // CSDVersion

        v.push(0xFE); // item START
        v.push(0x05); // group (non-ground, arbitrary)
        v.extend_from_slice(&(1u32 << 13).to_le_bytes()); // flags = FLAG_ALWAYSONTOP
        v.push(ITEM_ATTR_SERVER_ID);
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&200u16.to_le_bytes());
        v.push(ITEM_ATTR_CLIENT_ID);
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&1059u16.to_le_bytes());
        v.push(ITEM_ATTR_TOP_ORDER);
        v.extend_from_slice(&1u16.to_le_bytes()); // len = 1
        v.push(3u8); // top_order = 3
        v.push(0xFF); // item END

        v.push(0xFF); // root END

        let otb = parse(&v).unwrap();
        assert_eq!(otb.items.len(), 1);
        let it = &otb.items[0];
        assert!(it.always_on_top, "FLAG_ALWAYSONTOP set");
        assert_eq!(it.top_order, 3);
        assert!(!it.is_ground());
    }

    #[test]
    fn parses_version_and_one_item() {
        let otb = parse(&synthetic_otb()).unwrap();
        assert_eq!(otb.major_version, 3);
        assert_eq!(otb.minor_version, 57);
        assert_eq!(otb.build_number, 0);
        assert_eq!(otb.items.len(), 1);
        assert_eq!(
            otb.items[0],
            ItemType {
                group: 0x01,
                flags: 0x80,
                server_id: 100,
                client_id: 4526,
                always_on_top: false,
                top_order: 0,
            }
        );
    }
}
