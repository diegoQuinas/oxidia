//! Loader for `items.xml` (per-item attributes not in `items.otb`).
//!
//! M6.1 consumes only `floorChange`; the loader parses every `<attribute>` so
//! later milestones (M7 combat, M9 ground items, M10 inventory) can read the
//! rest. Ref: TFS `items.cpp::parseItemNode`.

/// Per-item floor-change directions, mirroring TFS `TILESTATE_FLOORCHANGE_*`
/// (`tile.h`). A staircase item carries one or more of these; the destination is
/// resolved by `world::map::StaticMap::resolve_floor_change`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FloorChange(u8);

impl FloorChange {
    pub const NONE: Self = Self(0);
    pub const DOWN: Self = Self(1 << 0);
    pub const NORTH: Self = Self(1 << 1);
    pub const SOUTH: Self = Self(1 << 2);
    pub const EAST: Self = Self(1 << 3);
    pub const WEST: Self = Self(1 << 4);
    pub const SOUTH_ALT: Self = Self(1 << 5);
    pub const EAST_ALT: Self = Self(1 << 6);

    /// True if `self` has all bits of `other` set (and `other` is non-empty).
    pub fn contains(self, other: Self) -> bool {
        other.0 != 0 && self.0 & other.0 == other.0
    }

    /// OR another flag in place (matches TFS `it.floorChange |= ...`).
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Map an `items.xml` `floorchange` value string to a flag, or `None` if
    /// unknown (TFS `items.cpp:153-159`).
    pub fn from_xml_value(value: &str) -> Option<Self> {
        match value {
            "down" => Some(Self::DOWN),
            "north" => Some(Self::NORTH),
            "south" => Some(Self::SOUTH),
            "southalt" => Some(Self::SOUTH_ALT),
            "west" => Some(Self::WEST),
            "east" => Some(Self::EAST),
            "eastalt" => Some(Self::EAST_ALT),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_change_string_table_matches_tfs() {
        assert_eq!(FloorChange::from_xml_value("down"), Some(FloorChange::DOWN));
        assert_eq!(
            FloorChange::from_xml_value("southalt"),
            Some(FloorChange::SOUTH_ALT)
        );
        assert_eq!(
            FloorChange::from_xml_value("eastalt"),
            Some(FloorChange::EAST_ALT)
        );
        assert_eq!(FloorChange::from_xml_value("nonsense"), None);
    }

    #[test]
    fn flags_or_and_contain() {
        let mut f = FloorChange::NONE;
        assert!(f.is_empty());
        f.insert(FloorChange::DOWN);
        f.insert(FloorChange::NORTH);
        assert!(f.contains(FloorChange::DOWN));
        assert!(f.contains(FloorChange::NORTH));
        assert!(!f.contains(FloorChange::SOUTH));
        assert!(!FloorChange::NONE.contains(FloorChange::NONE)); // empty contains nothing
    }
}
