//! Shared test fixtures for the game module's per-file test suites.

use super::*;
use formats::otb::{ItemType, ItemsOtb};
use formats::otbm::{MapItem, MapTile, OtbmMap, Town};

pub(super) fn stair_map() -> Arc<StaticMap> {
    use formats::items_xml::FloorChange;
    let items = ItemsOtb {
        major_version: 3, minor_version: 57, build_number: 0,
        items: vec![
            ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            ItemType { group: 5, flags: 0, server_id: 300, client_id: 1, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::DOWN },
        ],
    };
    let g = |x, y, z| MapTile { x, y, z, flags: 0, house_id: None, items: vec![MapItem { id: 100, count: None, contents: vec![] }] };
    let stair = |x, y, z| MapTile { x, y, z, flags: 0, house_id: None,
        items: vec![MapItem { id: 100, count: None, contents: vec![] }, MapItem { id: 300, count: None, contents: vec![] }] };
    let map = OtbmMap {
        width: 200, height: 200, major_items: 3, minor_items: 57,
        description: String::new(), spawn_file: None, house_file: None,
        tiles: vec![
            g(100, 100, 7),          // spawn
            stair(101, 100, 7),      // step east onto this -> floorchange down
            g(101, 100, 8),          // landing one floor below
        ],
        towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 7 }],
        waypoints: vec![],
    };
    Arc::new(StaticMap::from_formats(&map, &items))
}

pub(super) fn walk_map() -> Arc<StaticMap> {
    let items = ItemsOtb {
        major_version: 3, minor_version: 57, build_number: 0,
        items: vec![
            ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
            ItemType { group: 5, flags: 0x0000_0001, server_id: 200, client_id: 1059, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
        ],
    };
    let ground = |x, y| MapTile { x, y, z: 7, flags: 0, house_id: None,
        items: vec![MapItem { id: 100, count: None, contents: vec![] }] };
    let map = OtbmMap {
        width: 200, height: 200, major_items: 3, minor_items: 57,
        description: String::new(), spawn_file: None, house_file: None,
        tiles: vec![
            ground(95, 117), ground(96, 117), ground(95, 116),
            // wall to the west of spawn
            MapTile { x: 94, y: 117, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] }, MapItem { id: 200, count: None, contents: vec![] }] },
        ],
        towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
        waypoints: vec![],
    };
    Arc::new(StaticMap::from_formats(&map, &items))
}

pub(super) fn knight() -> Outfit {
    Outfit { look_type: 128, head: 78, body: 69, legs: 58, feet: 76, addons: 0, mount: 0 }
}

/// A custom outfit distinct from the default knight outfit, for restore tests.
pub(super) fn wizard_outfit() -> Outfit {
    Outfit { look_type: 75, head: 20, body: 30, legs: 40, feet: 50, addons: 1, mount: 0 }
}

/// Build a default `InitialState` for use in tests that don't care about
/// the restored-vs-new distinction.
pub(super) fn default_initial(outfit: Outfit) -> InitialState {
    InitialState {
        position: None,
        direction: Direction::South,
        outfit,
        health: 150,
        max_health: 150,
        sex: 1, // male (default)
        gamemaster: false,
        inventory: Vec::new(),
        container_items: Vec::new(),
    }
}

/// Insert a player at `pos` and return (id, its push receiver).
pub(super) fn add_player(g: &mut Game, pos: Position) -> (u32, mpsc::Receiver<Vec<u8>>) {
    let (tx, rx) = mpsc::channel(super::PUSH_CAPACITY);
    let id = g.next_id;
    g.next_id += 1;
    g.players.insert(id, PlayerState {
        name: "Tester".into(), position: pos, direction: Direction::South,
        outfit: knight(), push_tx: tx, known: HashSet::new(),
        health: 150, max_health: 150, fist_skill: 10,
        attacking: None, last_attack_ms: 0,
        sex: 1, // male (default)
        gamemaster: false,
        inventory: [None; 10],
        open_containers: std::array::from_fn(|_| None),
    });
    (id, rx)
}

pub(super) fn combat_map(spawn_pz: bool) -> Arc<StaticMap> {
    let items = ItemsOtb {
        major_version: 3, minor_version: 57, build_number: 0,
        items: vec![
            ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0, has_height: false, floor_change: formats::items_xml::FloorChange::NONE },
        ],
    };
    let ground = |x: u16, y: u16, pz: bool| MapTile {
        x, y, z: 7,
        flags: if pz { 1 } else { 0 }, // 1 = OTBM_TILEFLAG_PROTECTIONZONE
        house_id: None,
        items: vec![MapItem { id: 100, count: None, contents: vec![] }],
    };
    let map = OtbmMap {
        width: 200, height: 200, major_items: 3, minor_items: 57,
        description: String::new(), spawn_file: None, house_file: None,
        tiles: vec![
            ground(95, 117, spawn_pz), // spawn / temple
            ground(96, 117, false),    // adjacent east
            ground(97, 117, false),    // two tiles east
        ],
        towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
        waypoints: vec![],
    };
    Arc::new(StaticMap::from_formats(&map, &items))
}

pub(super) fn wide_combat_map_with_pz() -> Arc<StaticMap> {
    let items = ItemsOtb {
        major_version: 3, minor_version: 57, build_number: 0,
        items: vec![
            ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526,
                always_on_top: false, top_order: 0, has_height: false,
                floor_change: formats::items_xml::FloorChange::NONE },
        ],
    };
    let ground = |x: u16, y: u16, pz: bool| MapTile {
        x, y, z: 7,
        flags: if pz { 1 } else { 0 },
        house_id: None,
        items: vec![MapItem { id: 100, count: None, contents: vec![] }],
    };
    let mut tiles: Vec<MapTile> = (90u16..=116u16)
        .map(|x| ground(x, 117, x == 90))
        .collect();
    tiles.push(ground(115, 116, false));
    let map = OtbmMap {
        width: 200, height: 200, major_items: 3, minor_items: 57,
        description: String::new(), spawn_file: None, house_file: None,
        tiles,
        towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
        waypoints: vec![],
    };
    Arc::new(StaticMap::from_formats(&map, &items))
}

/// FLAG_PICKUPABLE (bit 5) from items.otb.
pub(super) const FLAG_PICKUPABLE_OTB: u32 = 1 << 5;
/// FLAG_STACKABLE (bit 7) from items.otb.
pub(super) const FLAG_STACKABLE_OTB: u32 = 1 << 7;

/// Build a map with item metadata loaded:
///
/// - server id 100: ground (group 1), no flags → not pickupable, not stackable
/// - server id 200: "stone" — pickupable, non-stackable, weight 110 (hundredths)
/// - server id 300: "gold coin" — pickupable + stackable, show_count true, weight 10
///
/// Tiles:
///
///   - (100,100,7) spawn — ground only
///   - (101,100,7) — ground + stone (sid 200) at index 1 (stackpos 1)
///   - (102,100,7) — ground + gold coin (sid 300, count 50) at index 1
///   - (103,100,7) — ground only (non-pickupable ground for weight-0 test)
pub(super) fn look_map() -> Arc<StaticMap> {
    use formats::items_xml::FloorChange;
    use formats::items_xml::ItemsXml;
    use formats::items_xml::parse_items_xml;
    use formats::otb::{ItemType as OtbItemType, ItemsOtb};
    use formats::otbm::{MapItem, MapTile, OtbmMap, Town};

    let otb = ItemsOtb {
        major_version: 3, minor_version: 57, build_number: 0,
        items: vec![
            // ground (group 1, no flags)
            OtbItemType { group: 1, flags: 0, server_id: 100, client_id: 4526,
                always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            // stone: pickupable (bit 5), not stackable
            OtbItemType { group: 5, flags: FLAG_PICKUPABLE_OTB, server_id: 200, client_id: 1987,
                always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            // gold coin: pickupable + stackable (bits 5+7)
            OtbItemType { group: 5, flags: FLAG_PICKUPABLE_OTB | FLAG_STACKABLE_OTB, server_id: 300,
                client_id: 2148, always_on_top: false, top_order: 0, has_height: false,
                floor_change: FloorChange::NONE },
        ],
    };

    let xml_str = r#"<items>
      <item id="200" name="stone" article="a" plural="stones">
        <attribute key="weight" value="110"/>
      </item>
      <item id="300" name="gold coin" article="a" plural="gold coins">
        <attribute key="weight" value="10"/>
        <attribute key="showcount" value="1"/>
      </item>
    </items>"#;
    let xml: ItemsXml = parse_items_xml(xml_str).unwrap();

    let g = |x: u16, y: u16| MapTile {
        x, y, z: 7, flags: 0, house_id: None,
        items: vec![MapItem { id: 100, count: None, contents: vec![] }],
    };
    let map = OtbmMap {
        width: 200, height: 200, major_items: 3, minor_items: 57,
        description: String::new(), spawn_file: None, house_file: None,
        tiles: vec![
            g(100, 100),  // spawn — ground only
            MapTile { x: 101, y: 100, z: 7, flags: 0, house_id: None,
                items: vec![
                    MapItem { id: 100, count: None, contents: vec![] },
                    MapItem { id: 200, count: None, contents: vec![] }, // stone at stackpos 1
                ] },
            MapTile { x: 102, y: 100, z: 7, flags: 0, house_id: None,
                items: vec![
                    MapItem { id: 100, count: None, contents: vec![] },
                    MapItem { id: 300, count: Some(50), contents: vec![] }, // 50 gold coins
                ] },
            g(103, 100),  // ground only
            g(99, 100),   // one tile west of spawn (for out-of-viewport test)
        ],
        towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 7 }],
        waypoints: vec![],
    };
    let mut sm = StaticMap::from_formats(&map, &otb);
    sm.load_item_metadata(&otb, &xml);
    Arc::new(sm)
}

/// Decode the text from a `0xB4 MESSAGE_INFO_DESCR` packet pushed to the
/// receiver. Panics if nothing was pushed or the format is wrong.
pub(super) fn recv_look_text(rx: &mut mpsc::Receiver<Vec<u8>>) -> String {
    let pkt = rx.try_recv().expect("expected a 0xB4 look packet");
    assert_eq!(pkt[0], 0xB4, "must be a 0xB4 text message");
    assert_eq!(pkt[1], MSG_INFO_DESCR, "must be MESSAGE_INFO_DESCR (22)");
    let len = u16::from_le_bytes([pkt[2], pkt[3]]) as usize;
    String::from_utf8(pkt[4..4 + len].to_vec()).expect("look text must be valid UTF-8")
}

/// FLAG_MOVEABLE = bit 6, FLAG_STACKABLE = bit 7, FLAG_PICKUPABLE = bit 5.
pub(super) const FLAG_MOVEABLE_OTB: u32 = 1 << 6;

/// Build a map ready for move-thing tests:
///
/// Items:
///   server 100 / client 4526 — ground (group 1, no flags)
///   server 200 / client 1987 — moveable stone (non-stackable)
///   server 300 / client 2148 — moveable gold coin (stackable)
///   server 400 / client 999  — non-moveable decoration (no FLAG_MOVEABLE)
///
/// Tiles (all z=7):
///   (100,100) — spawn / player start (ground only)
///   (101,100) — ground + stone (sid 200, stackpos 1)
///   (102,100) — ground + 10 gold coins (sid 300, count 10, stackpos 1)
///   (103,100) — ground + non-moveable deco (sid 400, stackpos 1)
///   (104,100) — empty (no tile — invalid dest)
///   (105,100) — ground only (valid empty dest)
pub(super) fn move_map() -> Arc<StaticMap> {
    use formats::items_xml::{FloorChange, ItemsXml, parse_items_xml};
    use formats::otb::{ItemType as OtbItemType, ItemsOtb};
    use formats::otbm::{MapItem, MapTile, OtbmMap, Town};

    let otb = ItemsOtb {
        major_version: 3, minor_version: 57, build_number: 0,
        items: vec![
            OtbItemType { group: 1, flags: 0, server_id: 100, client_id: 4526,
                always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            // stone: moveable, not stackable, not pickupable
            OtbItemType { group: 5, flags: FLAG_MOVEABLE_OTB, server_id: 200, client_id: 1987,
                always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            // gold coin: moveable + stackable + pickupable
            OtbItemType { group: 5, flags: FLAG_MOVEABLE_OTB | FLAG_STACKABLE_OTB | FLAG_PICKUPABLE_OTB,
                server_id: 300, client_id: 2148,
                always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            // decoration: NOT moveable (no FLAG_MOVEABLE)
            OtbItemType { group: 5, flags: 0, server_id: 400, client_id: 999,
                always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            // helmet: moveable + pickupable, slotType head (equippable in slot 1)
            OtbItemType { group: 5, flags: FLAG_MOVEABLE_OTB | FLAG_PICKUPABLE_OTB, server_id: 500, client_id: 5741,
                always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            // backpack: container (group 2), moveable + pickupable. Not on any tile.
            OtbItemType { group: 2, flags: FLAG_MOVEABLE_OTB | FLAG_PICKUPABLE_OTB, server_id: 600, client_id: 1988,
                always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
        ],
    };

    let xml_str = r#"<items>
      <item id="200" name="stone" article="a" plural="stones"><attribute key="weight" value="110"/></item>
      <item id="300" name="gold coin" article="a" plural="gold coins"><attribute key="weight" value="10"/><attribute key="showcount" value="1"/></item>
      <item id="400" name="decoration" article="a" plural="decorations"/>
      <item id="500" name="helmet" article="a"><attribute key="slotType" value="head"/></item>
      <item id="600" name="backpack" article="a"><attribute key="containersize" value="20"/></item>
    </items>"#;
    let xml: ItemsXml = parse_items_xml(xml_str).unwrap();

    let g = |x: u16| MapTile { x, y: 100, z: 7, flags: 0, house_id: None,
        items: vec![MapItem { id: 100, count: None, contents: vec![] }] };
    let map = OtbmMap {
        width: 200, height: 200, major_items: 3, minor_items: 57,
        description: String::new(), spawn_file: None, house_file: None,
        tiles: vec![
            g(100), // spawn
            MapTile { x: 101, y: 100, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] },
                            MapItem { id: 200, count: None, contents: vec![] }] }, // stone
            MapTile { x: 102, y: 100, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] },
                            MapItem { id: 300, count: Some(10), contents: vec![] }] }, // 10 coins
            MapTile { x: 103, y: 100, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] },
                            MapItem { id: 400, count: None, contents: vec![] }] }, // deco (non-moveable)
            // (104,100) deliberately absent — no tile → invalid dest
            g(105), // valid empty-item dest
            MapTile { x: 106, y: 100, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] },
                            MapItem { id: 500, count: None, contents: vec![] }] }, // helmet
            // Isolated vertical strip (y=110..113) for ground->ground container tests.
            MapTile { x: 100, y: 110, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] },
                            MapItem { id: 600, count: None, contents: vec![] }] }, // backpack on ground
            MapTile { x: 100, y: 111, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
            MapTile { x: 100, y: 112, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
            MapTile { x: 100, y: 113, z: 7, flags: 0, house_id: None,
                items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
        ],
        towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 7 }],
        waypoints: vec![],
    };
    let mut sm = StaticMap::from_formats(&map, &otb);
    sm.load_item_metadata(&otb, &xml);
    Arc::new(sm)
}

/// Helper: drain ALL pending packets from `rx` and return them.
pub(super) fn drain(rx: &mut mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Ok(p) = rx.try_recv() { out.push(p); }
    out
}

/// Helper: assert a packet list contains at least one packet whose first byte is `op`.
pub(super) fn has_op(packets: &[Vec<u8>], op: u8) -> bool {
    packets.iter().any(|p| p.first() == Some(&op))
}

/// Wire position for inventory slot `slot` (TFS: x=0xFFFF, y=slot, z=0).
pub(super) fn inv_pos(slot: u8) -> Position { Position::new(0xFFFF, u16::from(slot), 0) }

pub(super) fn outfit_window_looktypes(pkt: &[u8]) -> Vec<u16> {
    // [0xC8][AddOutfit current = 9][u8 count][per outfit: u16 lt, u16 namelen, name, u8 addons]...
    let mut p = 1 + 9;
    let count = pkt[p] as usize;
    p += 1;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let lt = u16::from_le_bytes([pkt[p], pkt[p + 1]]);
        p += 2;
        let name_len = u16::from_le_bytes([pkt[p], pkt[p + 1]]) as usize;
        p += 2 + name_len + 1; // namelen + name bytes + addons byte
        out.push(lt);
    }
    out
}

/// Drain a receiver and return the last `0xA2` icons packet seen, if any.
pub(super) fn drain_find_icons(rx: &mut mpsc::Receiver<Vec<u8>>) -> Option<Vec<u8>> {
    let mut found = None;
    while let Ok(pkt) = rx.try_recv() {
        if pkt.first() == Some(&enter_world::OP_ICONS) {
            found = Some(pkt);
        }
    }
    found
}

pub(super) fn count_sid_in_overlays(g: &Game, sid: u16) -> usize {
    g.dynamic.values()
        .map(|st| st.server_ids.iter().filter(|&&s| s == sid).count())
        .sum()
}
