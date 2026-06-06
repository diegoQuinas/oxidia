//! Parser for `.otbm` (the game map).
//!
//! The file is an OTB node tree (see [`crate::node`]):
//!
//! ```text
//! root  (props = OTBM_root_header)
//!  └─ MAP_DATA            (props = description / spawn-file / house-file attrs)
//!      ├─ TILE_AREA       (props = base x/y/z)        → TILE / HOUSETILE children
//!      ├─ TOWNS                                       → TOWN children
//!      └─ WAYPOINTS                                   → WAYPOINT children
//! ```
//!
//! Ref: TFS `iomap.cpp` / `iomap.h`.

use crate::node::Node;
use crate::props::PropReader;
use crate::FormatError;

// Node types (OTBM_NodeTypes_t).
const OTBM_MAP_DATA: u8 = 2;
const OTBM_TILE_AREA: u8 = 4;
const OTBM_TILE: u8 = 5;
const OTBM_ITEM: u8 = 6;
const OTBM_TOWNS: u8 = 12;
const OTBM_TOWN: u8 = 13;
const OTBM_HOUSETILE: u8 = 14;
const OTBM_WAYPOINTS: u8 = 15;
const OTBM_WAYPOINT: u8 = 16;

// Map-data / tile attributes (OTBM_AttrTypes_t).
const OTBM_ATTR_DESCRIPTION: u8 = 1;
const OTBM_ATTR_TILE_FLAGS: u8 = 3;
const OTBM_ATTR_ITEM: u8 = 9;
const OTBM_ATTR_EXT_SPAWN_FILE: u8 = 11;
const OTBM_ATTR_EXT_HOUSE_FILE: u8 = 13;

/// A fully parsed `.otbm` map.
#[derive(Debug, Clone)]
pub struct OtbmMap {
    /// Map width in tiles.
    pub width: u16,
    /// Map height in tiles.
    pub height: u16,
    /// `items.otb` major version the map was saved against.
    pub major_items: u32,
    /// `items.otb` minor version the map was saved against.
    pub minor_items: u32,
    /// Free-text map description (descriptions are concatenated with newlines).
    pub description: String,
    /// External spawn file referenced by the map, if any.
    pub spawn_file: Option<String>,
    /// External house file referenced by the map, if any.
    pub house_file: Option<String>,
    /// All non-empty tiles, in file order.
    pub tiles: Vec<MapTile>,
    /// Towns and their temple positions.
    pub towns: Vec<Town>,
    /// Named waypoints.
    pub waypoints: Vec<Waypoint>,
}

/// A single map tile with its absolute position and contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapTile {
    /// Absolute x (tile-area base + tile offset).
    pub x: u16,
    /// Absolute y.
    pub y: u16,
    /// Floor.
    pub z: u8,
    /// `OTBM_TileFlag_t` bitset (0 if none).
    pub flags: u32,
    /// `true` if this is a house tile.
    pub house_id: Option<u32>,
    /// Items on the tile, ground first, in file order.
    pub items: Vec<MapItem>,
}

/// An item on a tile (or inside a container). M2 keeps only the id and any
/// contained items; full attribute decoding is deferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapItem {
    /// Server item id.
    pub id: u16,
    /// Items contained within (for containers); empty otherwise.
    pub contents: Vec<MapItem>,
}

/// A town and its temple position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Town {
    /// Town id.
    pub id: u32,
    /// Town name.
    pub name: String,
    /// Temple x.
    pub x: u16,
    /// Temple y.
    pub y: u16,
    /// Temple floor.
    pub z: u8,
}

/// A named waypoint position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waypoint {
    /// Waypoint name.
    pub name: String,
    /// x.
    pub x: u16,
    /// y.
    pub y: u16,
    /// floor.
    pub z: u8,
}

/// Parse a full `.otbm` byte buffer.
pub fn parse(data: &[u8]) -> Result<OtbmMap, FormatError> {
    let root = crate::node::parse_tree(data)?;

    // Root props: OTBM_root_header { u32 version, u16 w, u16 h, u32 major, u32 minor }.
    let mut hdr = PropReader::new(&root.props);
    let _version = hdr.read_u32()?;
    let width = hdr.read_u16()?;
    let height = hdr.read_u16()?;
    let major_items = hdr.read_u32()?;
    let minor_items = hdr.read_u32()?;

    let map_data = root
        .children
        .iter()
        .find(|c| c.kind == OTBM_MAP_DATA)
        .ok_or(FormatError::InvalidNode { what: "missing OTBM_MAP_DATA node" })?;

    let mut map = OtbmMap {
        width,
        height,
        major_items,
        minor_items,
        description: String::new(),
        spawn_file: None,
        house_file: None,
        tiles: Vec::new(),
        towns: Vec::new(),
        waypoints: Vec::new(),
    };

    parse_map_data_attrs(&map_data.props, &mut map)?;

    for node in &map_data.children {
        match node.kind {
            OTBM_TILE_AREA => parse_tile_area(node, &mut map.tiles)?,
            OTBM_TOWNS => parse_towns(node, &mut map.towns)?,
            OTBM_WAYPOINTS => parse_waypoints(node, &mut map.waypoints)?,
            _ => return Err(FormatError::InvalidNode { what: "unknown map-data child node" }),
        }
    }

    Ok(map)
}

/// Read the MAP_DATA attribute stream (description, spawn/house file refs).
fn parse_map_data_attrs(props: &[u8], map: &mut OtbmMap) -> Result<(), FormatError> {
    let mut r = PropReader::new(props);
    while r.remaining() > 0 {
        let attr = r.read_u8()?;
        match attr {
            OTBM_ATTR_DESCRIPTION => {
                let desc = r.read_string()?;
                if map.description.is_empty() {
                    map.description = desc;
                } else {
                    map.description.push('\n');
                    map.description.push_str(&desc);
                }
            }
            OTBM_ATTR_EXT_SPAWN_FILE => map.spawn_file = Some(r.read_string()?),
            OTBM_ATTR_EXT_HOUSE_FILE => map.house_file = Some(r.read_string()?),
            _ => return Err(FormatError::InvalidNode { what: "unknown map-data attribute" }),
        }
    }
    Ok(())
}

/// Parse a TILE_AREA node and append its tiles.
fn parse_tile_area(node: &Node, out: &mut Vec<MapTile>) -> Result<(), FormatError> {
    let mut r = PropReader::new(&node.props);
    let base_x = r.read_u16()?;
    let base_y = r.read_u16()?;
    let z = r.read_u8()?;

    for tile_node in &node.children {
        if tile_node.kind != OTBM_TILE && tile_node.kind != OTBM_HOUSETILE {
            return Err(FormatError::InvalidNode { what: "unknown tile node" });
        }
        out.push(parse_tile(tile_node, base_x, base_y, z)?);
    }
    Ok(())
}

/// Parse one TILE / HOUSETILE node.
fn parse_tile(node: &Node, base_x: u16, base_y: u16, z: u8) -> Result<MapTile, FormatError> {
    let mut r = PropReader::new(&node.props);
    let x = base_x + r.read_u8()? as u16;
    let y = base_y + r.read_u8()? as u16;

    let house_id = if node.kind == OTBM_HOUSETILE { Some(r.read_u32()?) } else { None };

    let mut flags = 0;
    let mut items = Vec::new();
    while r.remaining() > 0 {
        let attr = r.read_u8()?;
        match attr {
            OTBM_ATTR_TILE_FLAGS => flags = r.read_u32()?,
            // Inline ground item: Item::CreateItem reads exactly a u16 id.
            OTBM_ATTR_ITEM => items.push(MapItem { id: r.read_u16()?, contents: vec![] }),
            _ => return Err(FormatError::InvalidNode { what: "unknown tile attribute" }),
        }
    }

    // Stacked items are stored as child OTBM_ITEM nodes.
    for item_node in &node.children {
        if item_node.kind != OTBM_ITEM {
            return Err(FormatError::InvalidNode { what: "unknown tile child node" });
        }
        items.push(parse_item(item_node)?);
    }

    Ok(MapTile { x, y, z, flags, house_id, items })
}

/// Parse an OTBM_ITEM node: leading u16 id, then (ignored) attributes, then
/// contained items as child nodes.
fn parse_item(node: &Node) -> Result<MapItem, FormatError> {
    let mut r = PropReader::new(&node.props);
    let id = r.read_u16()?;
    let mut contents = Vec::with_capacity(node.children.len());
    for child in &node.children {
        if child.kind != OTBM_ITEM {
            return Err(FormatError::InvalidNode { what: "unknown contained item node" });
        }
        contents.push(parse_item(child)?);
    }
    Ok(MapItem { id, contents })
}

/// Parse the TOWNS node.
fn parse_towns(node: &Node, out: &mut Vec<Town>) -> Result<(), FormatError> {
    for town_node in &node.children {
        if town_node.kind != OTBM_TOWN {
            return Err(FormatError::InvalidNode { what: "unknown town node" });
        }
        let mut r = PropReader::new(&town_node.props);
        let id = r.read_u32()?;
        let name = r.read_string()?;
        let x = r.read_u16()?;
        let y = r.read_u16()?;
        let z = r.read_u8()?;
        out.push(Town { id, name, x, y, z });
    }
    Ok(())
}

/// Parse the WAYPOINTS node.
fn parse_waypoints(node: &Node, out: &mut Vec<Waypoint>) -> Result<(), FormatError> {
    for wp_node in &node.children {
        if wp_node.kind != OTBM_WAYPOINT {
            return Err(FormatError::InvalidNode { what: "unknown waypoint node" });
        }
        let mut r = PropReader::new(&wp_node.props);
        let name = r.read_string()?;
        let x = r.read_u16()?;
        let y = r.read_u16()?;
        let z = r.read_u8()?;
        out.push(Waypoint { name, x, y, z });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small but structurally complete `.otbm`.
    fn synthetic_otbm() -> Vec<u8> {
        fn string(v: &mut Vec<u8>, s: &str) {
            v.extend_from_slice(&(s.len() as u16).to_le_bytes());
            v.extend_from_slice(s.as_bytes());
        }

        let mut v = vec![0x00, 0x00, 0x00, 0x00]; // identifier
        v.push(0xFE);
        v.push(0x00); // root type
        // OTBM_root_header
        v.extend_from_slice(&2u32.to_le_bytes()); // version
        v.extend_from_slice(&100u16.to_le_bytes()); // width
        v.extend_from_slice(&200u16.to_le_bytes()); // height
        v.extend_from_slice(&3u32.to_le_bytes()); // major items
        v.extend_from_slice(&57u32.to_le_bytes()); // minor items

        v.push(0xFE);
        v.push(OTBM_MAP_DATA);
        v.push(OTBM_ATTR_DESCRIPTION);
        string(&mut v, "test map");
        v.push(OTBM_ATTR_EXT_SPAWN_FILE);
        string(&mut v, "spawn.xml");
        v.push(OTBM_ATTR_EXT_HOUSE_FILE);
        string(&mut v, "house.xml");

        // TILE_AREA at base (10, 20, 7)
        v.push(0xFE);
        v.push(OTBM_TILE_AREA);
        v.extend_from_slice(&10u16.to_le_bytes());
        v.extend_from_slice(&20u16.to_le_bytes());
        v.push(7);
        // TILE at offset (1, 2) -> abs (11, 22)
        v.push(0xFE);
        v.push(OTBM_TILE);
        v.push(1);
        v.push(2);
        v.push(OTBM_ATTR_TILE_FLAGS);
        v.extend_from_slice(&1u32.to_le_bytes()); // PROTECTIONZONE
        v.push(OTBM_ATTR_ITEM);
        v.extend_from_slice(&4526u16.to_le_bytes()); // inline ground item
        // child item node (e.g. a stacked item)
        v.push(0xFE);
        v.push(OTBM_ITEM);
        v.extend_from_slice(&1234u16.to_le_bytes());
        v.push(0xFF); // end child item
        v.push(0xFF); // end tile
        v.push(0xFF); // end tile area

        // TOWNS
        v.push(0xFE);
        v.push(OTBM_TOWNS);
        v.push(0xFE);
        v.push(OTBM_TOWN);
        v.extend_from_slice(&1u32.to_le_bytes());
        string(&mut v, "Thais");
        v.extend_from_slice(&15u16.to_le_bytes());
        v.extend_from_slice(&25u16.to_le_bytes());
        v.push(7);
        v.push(0xFF); // end town
        v.push(0xFF); // end towns

        // WAYPOINTS
        v.push(0xFE);
        v.push(OTBM_WAYPOINTS);
        v.push(0xFE);
        v.push(OTBM_WAYPOINT);
        string(&mut v, "temple");
        v.extend_from_slice(&15u16.to_le_bytes());
        v.extend_from_slice(&25u16.to_le_bytes());
        v.push(7);
        v.push(0xFF); // end waypoint
        v.push(0xFF); // end waypoints

        v.push(0xFF); // end map data
        v.push(0xFF); // end root
        v
    }

    fn reference_otbm() -> Option<Vec<u8>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/tfs/data/world/forgotten.otbm");
        path.exists().then(|| std::fs::read(path).unwrap())
    }

    #[test]
    fn parses_the_real_forgotten_map() {
        let Some(data) = reference_otbm() else {
            eprintln!("skipping: reference/tfs not present");
            return;
        };
        let map = parse(&data).unwrap();
        assert_eq!((map.width, map.height), (2048, 2048));
        assert_eq!(map.major_items, 3);
        assert_eq!(map.minor_items, 57);
        assert!(map.tiles.len() > 1000, "got {} tiles", map.tiles.len());
        assert!(!map.towns.is_empty(), "expected towns");
        // Every tile should carry at least a ground item.
        assert!(map.tiles.iter().all(|t| !t.items.is_empty()));
    }

    #[test]
    fn parses_the_header() {
        let map = parse(&synthetic_otbm()).unwrap();
        assert_eq!(map.width, 100);
        assert_eq!(map.height, 200);
        assert_eq!(map.major_items, 3);
        assert_eq!(map.minor_items, 57);
    }

    #[test]
    fn parses_map_data_attributes() {
        let map = parse(&synthetic_otbm()).unwrap();
        assert_eq!(map.description, "test map");
        assert_eq!(map.spawn_file.as_deref(), Some("spawn.xml"));
        assert_eq!(map.house_file.as_deref(), Some("house.xml"));
    }

    #[test]
    fn parses_a_tile_with_inline_and_child_items() {
        let map = parse(&synthetic_otbm()).unwrap();
        assert_eq!(map.tiles.len(), 1);
        let tile = &map.tiles[0];
        assert_eq!((tile.x, tile.y, tile.z), (11, 22, 7));
        assert_eq!(tile.flags, 1);
        assert_eq!(tile.house_id, None);
        assert_eq!(tile.items.len(), 2);
        assert_eq!(tile.items[0], MapItem { id: 4526, contents: vec![] });
        assert_eq!(tile.items[1], MapItem { id: 1234, contents: vec![] });
    }

    #[test]
    fn parses_towns_and_waypoints() {
        let map = parse(&synthetic_otbm()).unwrap();
        assert_eq!(
            map.towns,
            vec![Town { id: 1, name: "Thais".into(), x: 15, y: 25, z: 7 }]
        );
        assert_eq!(
            map.waypoints,
            vec![Waypoint { name: "temple".into(), x: 15, y: 25, z: 7 }]
        );
    }
}
