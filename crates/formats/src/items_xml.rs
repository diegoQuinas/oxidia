//! Loader for `items.xml` (per-item attributes not in `items.otb`).
//!
//! M6.1 consumes only `floorChange`; the loader parses every `<attribute>` so
//! later milestones (M7 combat, M9 ground items, M10 inventory) can read the
//! rest. Ref: TFS `items.cpp::parseItemNode`.

use std::collections::HashMap;

use crate::FormatError;

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

/// Per-item attributes loaded from `items.xml`, keyed by server id.
#[derive(Debug, Clone)]
pub struct ItemXmlAttrs {
    pub floor_change: FloorChange,
    /// Display name (`<item name=…>`); empty if absent.
    pub name: String,
    /// Indefinite article, `"a"` / `"an"` (`<item article=…>`).
    pub article: String,
    /// Plural name (`<item plural=…>`), used for stackable count > 1.
    pub plural: String,
    /// Look description (`<attribute key="description">`).
    pub description: String,
    /// Weight in **hundredths of an oz** (`<attribute key="weight">`).
    pub weight: u32,
    /// Whether the count prefixes the name for stacks (`showcount`, default true).
    pub show_count: bool,
    /// Equip slot type (`<attribute key="slotType">`): "head","body","legs",
    /// "feet","necklace","ring","ammo","backpack","two-handed", or empty.
    pub slot_type: String,
    /// Weapon type (`<attribute key="weaponType">`): "sword","axe","club",
    /// "distance","wand","shield","ammunition", or empty.
    pub weapon_type: String,
    /// Container capacity (`<attribute key="containersize">`). 0 if not a container.
    pub container_size: u8,
}

impl Default for ItemXmlAttrs {
    fn default() -> Self {
        Self {
            floor_change: FloorChange::NONE,
            name: String::new(),
            article: String::new(),
            plural: String::new(),
            description: String::new(),
            weight: 0,
            show_count: true,
            slot_type: String::new(),
            weapon_type: String::new(),
            container_size: 0,
        }
    }
}

/// All `items.xml` attributes, indexed by server id.
#[derive(Debug, Clone, Default)]
pub struct ItemsXml {
    by_server_id: HashMap<u16, ItemXmlAttrs>,
}

impl ItemsXml {
    /// The floor-change flags for `server_id`, or `NONE` if absent.
    pub fn floor_change(&self, server_id: u16) -> FloorChange {
        self.by_server_id
            .get(&server_id)
            .map_or(FloorChange::NONE, |a| a.floor_change)
    }

    pub fn attrs(&self, server_id: u16) -> Option<&ItemXmlAttrs> {
        self.by_server_id.get(&server_id)
    }
}

/// Parse an `items.xml` document. Walks `<item id|fromid/toid>` and every nested
/// `<attribute key value>`. Unknown attributes are ignored (forward-compatible);
/// an unknown `floorchange` value is ignored too (TFS warns and continues).
pub fn parse_items_xml(xml: &str) -> Result<ItemsXml, FormatError> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| FormatError::InvalidNode {
        what: "items.xml is not well-formed",
    })?;
    let mut by_server_id: HashMap<u16, ItemXmlAttrs> = HashMap::new();

    for item in doc.descendants().filter(|n| n.has_tag_name("item")) {
        let ids = item_id_range(&item);
        if ids.is_empty() {
            continue;
        }
        let mut attrs = ItemXmlAttrs::default();
        // Element attributes on <item …>.
        if let Some(v) = item.attribute("name") { attrs.name = v.to_string(); }
        if let Some(v) = item.attribute("article") { attrs.article = v.to_string(); }
        if let Some(v) = item.attribute("plural") { attrs.plural = v.to_string(); }
        // <attribute key=… value=…> children.
        for attr in item.children().filter(|n| n.has_tag_name("attribute")) {
            let key = attr.attribute("key").unwrap_or("");
            let value = attr.attribute("value").unwrap_or("");
            if key.eq_ignore_ascii_case("floorchange") {
                if let Some(fc) = FloorChange::from_xml_value(value) {
                    attrs.floor_change.insert(fc);
                }
            } else if key.eq_ignore_ascii_case("description") {
                attrs.description = value.to_string();
            } else if key.eq_ignore_ascii_case("weight") {
                attrs.weight = value.parse::<u32>().unwrap_or(0);
            } else if key.eq_ignore_ascii_case("showcount") {
                attrs.show_count = !value.eq_ignore_ascii_case("0")
                    && !value.eq_ignore_ascii_case("false");
            } else if key.eq_ignore_ascii_case("slotType") {
                attrs.slot_type = value.to_ascii_lowercase();
            } else if key.eq_ignore_ascii_case("weaponType") {
                attrs.weapon_type = value.to_ascii_lowercase();
            } else if key.eq_ignore_ascii_case("containersize") {
                attrs.container_size = value.parse::<u8>().unwrap_or(0);
            }
        }
        for id in ids {
            let entry = by_server_id.entry(id).or_default();
            entry.floor_change.insert(attrs.floor_change);
            if !attrs.name.is_empty() { entry.name = attrs.name.clone(); }
            if !attrs.article.is_empty() { entry.article = attrs.article.clone(); }
            if !attrs.plural.is_empty() { entry.plural = attrs.plural.clone(); }
            if !attrs.description.is_empty() { entry.description = attrs.description.clone(); }
            if attrs.weight != 0 { entry.weight = attrs.weight; }
            entry.show_count = attrs.show_count;
            if !attrs.slot_type.is_empty() { entry.slot_type = attrs.slot_type.clone(); }
            if !attrs.weapon_type.is_empty() { entry.weapon_type = attrs.weapon_type.clone(); }
            if attrs.container_size != 0 { entry.container_size = attrs.container_size; }
        }
    }

    Ok(ItemsXml { by_server_id })
}

/// The server ids an `<item>` element covers: a single `id`, or the inclusive
/// `fromid..=toid` range.
fn item_id_range(item: &roxmltree::Node) -> Vec<u16> {
    if let Some(id) = item.attribute("id").and_then(|s| s.parse::<u16>().ok()) {
        return vec![id];
    }
    match (
        item.attribute("fromid").and_then(|s| s.parse::<u16>().ok()),
        item.attribute("toid").and_then(|s| s.parse::<u16>().ok()),
    ) {
        (Some(from), Some(to)) if from <= to => (from..=to).collect(),
        _ => Vec::new(),
    }
}

/// Merge `items.xml` attributes into the `items.otb` table by server id. After
/// this, each `ItemType.floor_change` reflects its `items.xml` entry.
pub fn merge_items_xml(items: &mut crate::otb::ItemsOtb, xml: &ItemsXml) {
    for it in &mut items.items {
        let fc = xml.floor_change(it.server_id);
        if !fc.is_empty() {
            it.floor_change = fc;
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

    #[test]
    fn parses_floor_change_for_id_and_range() {
        let xml = r#"
        <items>
          <item id="1386" name="stairs"><attribute key="floorchange" value="down"/></item>
          <item fromid="461" toid="462" name="ladder up"><attribute key="floorchange" value="north"/></item>
        </items>"#;
        let parsed = parse_items_xml(xml).unwrap();
        assert_eq!(parsed.floor_change(1386), FloorChange::DOWN);
        assert_eq!(parsed.floor_change(461), FloorChange::NORTH);
        assert_eq!(parsed.floor_change(462), FloorChange::NORTH);
        assert_eq!(parsed.floor_change(9999), FloorChange::NONE); // absent
    }

    #[test]
    fn merge_sets_floor_change_on_item_type() {
        use crate::otb::{ItemType, ItemsOtb};
        let mut items = ItemsOtb {
            major_version: 3,
            minor_version: 57,
            build_number: 0,
            items: vec![ItemType {
                group: 1,
                flags: 0,
                server_id: 1386,
                client_id: 1,
                always_on_top: false,
                top_order: 0,
                has_height: false,
                floor_change: FloorChange::NONE,
            }],
        };
        let xml = parse_items_xml(
            r#"<items><item id="1386"><attribute key="floorchange" value="down"/></item></items>"#,
        )
        .unwrap();
        merge_items_xml(&mut items, &xml);
        assert_eq!(items.items[0].floor_change, FloorChange::DOWN);
    }

    fn reference_items_xml() -> Option<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/tfs/data/items/items.xml");
        path.exists()
            .then(|| std::fs::read_to_string(path).unwrap())
    }

    #[test]
    fn parses_the_real_items_xml() {
        let Some(xml) = reference_items_xml() else {
            eprintln!("skipping: reference/tfs not present");
            return;
        };
        let parsed = parse_items_xml(&xml).unwrap();
        // The real map has staircases: at least one item carries DOWN.
        let any_down = (1..=30000u16).any(|id| parsed.floor_change(id).contains(FloorChange::DOWN));
        assert!(
            any_down,
            "real items.xml should define floorchange=down stairs"
        );
    }

    #[test]
    fn item_attrs_name_article_plural_description_weight() {
        let xml = r#"<items>
          <item id="2148" name="gold coin" article="a" plural="gold coins">
            <attribute key="description" value="Shiny!"/>
            <attribute key="weight" value="550"/>
          </item>
        </items>"#;
        let parsed = parse_items_xml(xml).unwrap();
        let a = parsed.attrs(2148).expect("item 2148 must be present");
        assert_eq!(a.name, "gold coin");
        assert_eq!(a.article, "a");
        assert_eq!(a.plural, "gold coins");
        assert_eq!(a.description, "Shiny!");
        // Weight is stored in hundredths of an oz as-is (NOT divided).
        assert_eq!(a.weight, 550);
        // show_count defaults to true when no showcount attribute is present.
        assert!(a.show_count, "show_count must default to true");
    }

    #[test]
    fn item_attrs_showcount_false_when_zero() {
        let xml = r#"<items>
          <item id="1987" name="stone" article="a" plural="stones">
            <attribute key="weight" value="110"/>
            <attribute key="showcount" value="0"/>
          </item>
        </items>"#;
        let parsed = parse_items_xml(xml).unwrap();
        let a = parsed.attrs(1987).expect("item 1987 must be present");
        assert!(!a.show_count, "showcount=0 must yield show_count == false");
    }

    #[test]
    fn absent_item_attrs_returns_none() {
        let xml = r#"<items><item id="100" name="grass"/></items>"#;
        let parsed = parse_items_xml(xml).unwrap();
        assert!(parsed.attrs(9999).is_none(), "absent item must return None");
    }
}
