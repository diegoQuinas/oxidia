//! Stationary NPCs placed from the real-map spawn file.
//!
//! TFS `Spawns::loadFromXml` puts an NPC at `(centerx + x, centery + y, centerz)`
//! and ignores the child's `z` (that attribute always echoes `centerz` in
//! `map1-spawn.xml`; adding the two doubles the floor). Outfits are not in this
//! datapack — `data/npc/` was never shipped — so each NPC gets a citizen look
//! with colors derived from the name until the real outfit pack is mined.
//! They do not walk, fight, respawn, or talk.

use protocol::creature::Outfit;

use crate::{Direction, Position};
use formats::FormatError;

/// First NPC id. Players start at `0x1000_0000`, monsters at `0x4000_0000`.
pub(crate) const NPC_ID_BASE: u32 = 0x8000_0000;

/// A placed NPC. No combat state: they only occupy a tile and face a direction.
#[derive(Debug, Clone)]
pub(crate) struct NpcState {
    pub(crate) name: String,
    pub(crate) position: Position,
    pub(crate) direction: Direction,
    pub(crate) outfit: Outfit,
}

/// One `<npc>` row, already resolved to an absolute tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NpcPlacement {
    pub(crate) name: String,
    pub(crate) position: Position,
    pub(crate) direction: Direction,
}

/// Parse `<npc>` children of `<spawn>`. Monster rows in the same file are ignored.
///
/// Position is TFS's: x/y are offsets from the spawn center, z is the center
/// floor. A missing `direction` is north, matching `Spawns::loadFromXml`.
pub(crate) fn parse_npc_spawns_xml(xml: &str) -> Result<Vec<NpcPlacement>, FormatError> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| FormatError::InvalidNode {
        what: "spawns.xml is not well-formed",
    })?;
    let mut npcs = Vec::new();
    for spawn_node in doc.descendants().filter(|n| n.has_tag_name("spawn")) {
        let cx: u16 = spawn_node
            .attribute("centerx")
            .and_then(|s| s.parse().ok())
            .ok_or(FormatError::InvalidNode {
                what: "spawn missing centerx",
            })?;
        let cy: u16 = spawn_node
            .attribute("centery")
            .and_then(|s| s.parse().ok())
            .ok_or(FormatError::InvalidNode {
                what: "spawn missing centery",
            })?;
        let cz: u8 = spawn_node
            .attribute("centerz")
            .and_then(|s| s.parse().ok())
            .ok_or(FormatError::InvalidNode {
                what: "spawn missing centerz",
            })?;
        for npc_node in spawn_node.children().filter(|n| n.has_tag_name("npc")) {
            let name = npc_node
                .attribute("name")
                .ok_or(FormatError::InvalidNode {
                    what: "npc in spawn missing name",
                })?
                .to_string();
            let dx: i16 = npc_node
                .attribute("x")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let dy: i16 = npc_node
                .attribute("y")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let x = i32::from(cx).wrapping_add(i32::from(dx)) as u16;
            let y = i32::from(cy).wrapping_add(i32::from(dy)) as u16;
            // Child `z` is not an offset. TFS places the NPC on `centerz`.
            npcs.push(NpcPlacement {
                name,
                position: Position::new(x, y, cz),
                direction: direction_from_attr(npc_node.attribute("direction")),
            });
        }
    }
    Ok(npcs)
}

/// Citizen look so a town reads as people, not a monster sprite. Colors come
/// from the name so two NPCs on the same street are not clones. Real looks
/// land when `data/npc/*.xml` is available.
pub(crate) fn npc_outfit(name: &str) -> Outfit {
    let h = name.bytes().fold(0u32, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u32::from(b))
    });
    let color = |shift: u32| ((h >> shift) % 133) as u8;
    Outfit {
        look_type: if h % 2 == 0 { 128 } else { 136 },
        head: color(0),
        body: color(8),
        legs: color(16),
        feet: color(24),
        addons: 0,
        mount: 0,
    }
}

fn direction_from_attr(raw: Option<&str>) -> Direction {
    match raw.and_then(|s| s.parse::<u8>().ok()) {
        Some(1) => Direction::East,
        Some(2) => Direction::South,
        Some(3) => Direction::West,
        Some(4) => Direction::SouthWest,
        Some(5) => Direction::SouthEast,
        Some(6) => Direction::NorthWest,
        Some(7) => Direction::NorthEast,
        _ => Direction::North,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npc_position_uses_center_floor_not_child_z() {
        // Real file: child z always equals centerz. Adding them would put Sam
        // on z 14. A mismatch must still follow TFS and keep centerz.
        let xml = r#"
            <spawns>
              <spawn centerx="32361" centery="32198" centerz="7" radius="1">
                <npc name="Sam" x="0" y="0" z="7" spawntime="60" direction="2" />
              </spawn>
              <spawn centerx="100" centery="100" centerz="7" radius="1">
                <npc name="Odd" x="1" y="-2" z="0" direction="1" />
              </spawn>
              <spawn centerx="32658" centery="31916" centerz="0" radius="3">
                <npc name="UzonE" x="1" y="-3" z="0" spawntime="60" direction="2" />
                <monster name="Rat" x="0" y="0" z="0" spawntime="60" />
              </spawn>
            </spawns>
        "#;
        let npcs = parse_npc_spawns_xml(xml).unwrap();
        assert_eq!(npcs.len(), 3, "monster rows are not NPCs");
        assert_eq!(npcs[0].name, "Sam");
        assert_eq!(npcs[0].position, Position::new(32361, 32198, 7));
        assert_eq!(npcs[0].direction, Direction::South);
        assert_eq!(npcs[1].position, Position::new(101, 98, 7));
        assert_eq!(npcs[1].direction, Direction::East);
        assert_eq!(npcs[2].position, Position::new(32659, 31913, 0));
    }

    #[test]
    fn missing_direction_defaults_to_north() {
        let xml = r#"
            <spawns>
              <spawn centerx="1" centery="2" centerz="3" radius="1">
                <npc name="Silent" x="0" y="0" z="3" />
              </spawn>
            </spawns>
        "#;
        let npcs = parse_npc_spawns_xml(xml).unwrap();
        assert_eq!(npcs[0].direction, Direction::North);
        assert_eq!(npcs[0].position, Position::new(1, 2, 3));
    }
}

#[cfg(test)]
mod placement {
    use super::super::test_support::{
        add_player, default_initial, knight, recv_look_text, walk_map,
    };
    use super::super::*;

    const SAM: &str = r#"
        <spawns>
          <spawn centerx="96" centery="117" centerz="7" radius="1">
            <npc name="Sam" x="0" y="0" z="7" spawntime="60" direction="2" />
          </spawn>
          <spawn centerx="200" centery="200" centerz="7" radius="1">
            <npc name="Far" x="0" y="0" z="7" direction="0" />
          </spawn>
        </spawns>
    "#;

    #[test]
    fn load_npcs_places_on_center_floor_and_does_not_spawn_monsters() {
        let mut g = Game::from_static_map_arc(walk_map());
        g.load_npcs(SAM);
        assert_eq!(g.npcs.len(), 2);
        assert!(
            g.monsters.is_empty(),
            "an <npc> row must not become a monster"
        );
        let sam = g.npcs.values().find(|n| n.name == "Sam").unwrap();
        assert_eq!(sam.position, Position::new(96, 117, 7));
        assert_eq!(sam.direction, Direction::South);
        assert!(sam.outfit.look_type == 128 || sam.outfit.look_type == 136);
        assert!(g.tile_occupied(Position::new(96, 117, 7), u32::MAX));
        let id = g.npcs.iter().find(|(_, n)| n.name == "Sam").unwrap().0;
        assert_eq!(
            g.creature_type_byte(*id),
            protocol::creature::CREATURETYPE_NPC
        );
        assert!(
            !g.creature_exists(*id),
            "NPCs are not combatants — attacking them must be rejected"
        );
    }

    #[test]
    fn login_includes_visible_npc_as_creaturetype_npc() {
        let mut g = Game::from_static_map_arc(walk_map());
        g.load_npcs(SAM);
        let (tx, _rx) = mpsc::channel(super::super::PUSH_CAPACITY);
        let ack = g.login("Hero".into(), default_initial(knight()), tx);
        let npc_ct = protocol::creature::CREATURETYPE_NPC;
        let sam = ack.others.iter().find(|pc| {
            pc.bytes.len() > 10 && pc.bytes[10] == npc_ct && pc.x == 96 && pc.y == 117 && pc.z == 7
        });
        let sam = sam.expect("login must place Sam on his tile");
        // "Sam" is 3 bytes, same layout as the Rat monster test: second type byte at 34.
        assert_eq!(sam.bytes[34], npc_ct, "both creatureType bytes must be NPC");
        assert!(
            !ack.others.iter().any(|pc| pc.x == 200 && pc.y == 200),
            "an NPC outside the viewport must not be in the login burst"
        );
    }

    #[test]
    fn look_at_npc_says_the_name() {
        let mut g = Game::from_static_map_arc(walk_map());
        g.load_npcs(SAM);
        let (looker, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        // Ground is stackpos 0; the NPC is the first creature, stackpos 1.
        g.do_look(looker, 96, 117, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(text.contains("You see Sam."), "text: {text:?}");
    }
}
