//! Monster state and type definitions for the game actor.
//!
//! Monsters are server-side creatures without client sessions.
//! They share the same ID namespace with players but use the
//! `0x4000_0000+` range. All monster state lives here; the `Game`
//! struct holds a `HashMap<u32, MonsterState>`.
//!
//! ## Monster types
//!
//! The `config/monsters.xml` file defines named monster blueprints
//! (looktype, health, speed, attack). Spawn entries in `world/map-spawn.xml`
//! reference these names; unknown names fall back to hardcoded defaults.

use std::collections::{HashMap, VecDeque};

use crate::Direction;
use crate::Position;
use formats::FormatError;

/// A monster blueprint loaded from `config/monsters.xml`.
/// These are the canonical stats for each named monster type.
#[derive(Debug, Clone)]
pub(crate) struct MonsterType {
    pub(crate) name: String,
    pub(crate) look_type: u16,
    pub(crate) health: u32,
    pub(crate) max_health: u32,
    pub(crate) speed: u16,
    pub(crate) attack: u16,
    pub(crate) loot: Vec<MonsterDrop>,
    /// Preferred distance from target (0 = melee, 1+ = ranged).
    pub(crate) target_distance: i32,
}

/// Parse `<monsters>` XML and return a name-indexed map.
///
/// Expected format:
/// ```xml
/// <monsters>
///   <monster name="Rat" looktype="100" health="50" max_health="50" speed="200" attack="7"/>
/// </monsters>
/// ```
pub(crate) fn parse_monsters_xml(xml: &str) -> Result<HashMap<String, MonsterType>, FormatError> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| FormatError::InvalidNode {
        what: "monsters.xml is not well-formed",
    })?;
    let mut map = HashMap::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("monster")) {
        let name = node
            .attribute("name")
            .ok_or(FormatError::InvalidNode {
                what: "monster missing name attribute",
            })?
            .to_string();
        let look_type = node
            .attribute("looktype")
            .and_then(|s| s.parse().ok())
            .ok_or(FormatError::InvalidNode {
                what: "monster missing or invalid looktype attribute",
            })?;
        let health = node
            .attribute("health")
            .and_then(|s| s.parse().ok())
            .ok_or(FormatError::InvalidNode {
                what: "monster missing or invalid health attribute",
            })?;
        let max_health = node
            .attribute("max_health")
            .and_then(|s| s.parse().ok())
            .unwrap_or(health);
        let speed = node
            .attribute("speed")
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        let attack = node
            .attribute("attack")
            .and_then(|s| s.parse().ok())
            .unwrap_or(7);
        map.insert(
            name.to_ascii_lowercase(),
            MonsterType {
                name,
                look_type,
                health,
                max_health,
                speed,
                attack,
                loot: vec![],
                target_distance: 0,
            },
        );
    }
    Ok(map)
}

/// Parse `<spawns>` XML and return a list of spawn entries.
///
/// Expected format:
/// ```xml
/// <spawns>
///   <spawn centerx="32153" centery="31124" centerz="0" radius="2">
///     <monster name="Silver Rabbit" x="0" y="1" z="0" spawntime="60"/>
///   </spawn>
/// </spawns>
/// ```
pub(crate) fn parse_spawns_xml(xml: &str) -> Result<Vec<MonsterSpawn>, FormatError> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| FormatError::InvalidNode {
        what: "spawns.xml is not well-formed",
    })?;
    let mut spawns = Vec::new();
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
        for monster_node in spawn_node.children().filter(|n| n.has_tag_name("monster")) {
            let name = monster_node
                .attribute("name")
                .ok_or(FormatError::InvalidNode {
                    what: "monster in spawn missing name",
                })?
                .to_string();
            let dx: i16 = monster_node
                .attribute("x")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let dy: i16 = monster_node
                .attribute("y")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let dz: i8 = monster_node
                .attribute("z")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let spawntime_s: u64 = monster_node
                .attribute("spawntime")
                .and_then(|s| s.parse().ok())
                .unwrap_or(60);
            let x = (i32::from(cx)).wrapping_add(i32::from(dx)) as u16;
            let y = (i32::from(cy)).wrapping_add(i32::from(dy)) as u16;
            let z = (i32::from(cz)).wrapping_add(i32::from(dz)) as u8;
            let position = Position::new(x, y, z);
            spawns.push(MonsterSpawn {
                position,
                respawn_interval_ms: spawntime_s * 1000,
                respawn_at_ms: None,
                name: name.clone(),
                look_type: name_hash_looktype(&name),
                health: 50,
                max_health: 50,
                speed: 200,
                attack: 7,
                loot: vec![],
                target_distance: 0,
            });
        }
    }
    Ok(spawns)
}

/// A single drop entry in a monster's loot table.
/// When the monster dies, each entry is rolled independently.
#[derive(Debug, Clone)]
pub(crate) struct MonsterDrop {
    /// Server-side item id (resolved via `map.item_meta()`).
    pub(crate) item_id: u16,
    /// Probability 0.0..=1.0 for this drop to trigger.
    pub(crate) chance: f64,
    /// Item count (subtype for stackables, 1 for non-stackables).
    pub(crate) count: u8,
}

/// Blueprint for a monster respawn point.
/// Stored in `Game::spawns` and linked to a living monster via
/// `MonsterState::spawn_id`. When the monster dies, `respawn_at_ms`
/// is set; the next `on_combat_tick` checks for overdue spawns.
#[derive(Debug, Clone)]
pub(crate) struct MonsterSpawn {
    /// Where to place the respawned monster.
    pub(crate) position: Position,
    /// Millisecond delay before respawning after death.
    pub(crate) respawn_interval_ms: u64,
    /// When the respawn is due (`None` = no pending respawn).
    pub(crate) respawn_at_ms: Option<u64>,
    /// Monster template fields carried over from the dead monster.
    pub(crate) name: String,
    pub(crate) look_type: u16,
    pub(crate) health: u32,
    pub(crate) max_health: u32,
    pub(crate) speed: u16,
    pub(crate) attack: u16,
    pub(crate) loot: Vec<MonsterDrop>,
    /// Preferred distance from target (0 = melee, 1+ = ranged).
    pub(crate) target_distance: i32,
}

/// Runtime state for a single monster.
/// Mirrors the subset of `PlayerState` relevant to non-player creatures.
#[derive(Debug, Clone)]
pub(crate) struct MonsterState {
    pub(crate) name: String,
    pub(crate) position: Position,
    pub(crate) direction: Direction,
    pub(crate) health: u32,
    pub(crate) max_health: u32,
    pub(crate) speed: u16,
    /// Sprite id for the monster's look (no head/body/legs/feet/addons/mount).
    pub(crate) look_type: u16,
    /// Combat state.
    pub(crate) attacking: Option<u32>,
    /// Timestamp (ms) of the monster's last melee swing.
    pub(crate) last_attack_ms: u64,
    /// Base physical melee damage (max per swing).
    pub(crate) attack: u16,
    /// Items dropped on death. Rolled independently in `do_monster_death`.
    pub(crate) loot: Vec<MonsterDrop>,
    /// Id of the spawn entry this monster belongs to, if any.
    /// When set and the spawn's `respawn_interval_ms > 0`, the monster
    /// automatically respawns after death.
    pub(crate) spawn_id: Option<u32>,
    /// Auto-walk directions queued by A* pathfinding. Consumed one per AI tick.
    pub(crate) list_walk_dir: VecDeque<Direction>,
    /// Creature id this monster is chasing, if any.
    pub(crate) follow_target: Option<u32>,
    /// Desired distance from target (1 = melee adjacent, N = ranged).
    pub(crate) target_distance: i32,
}

impl MonsterState {
    /// Health percent as a 0..100 integer, matching the wire format.
    pub(crate) fn health_percent(&self) -> u8 {
        if self.max_health == 0 {
            return 0;
        }
        ((self.health * 100) / self.max_health).min(100) as u8
    }
}

/// Derive a deterministic looktype from a name string.
/// Use for unknown monster types so they look different from each other.
pub(crate) fn name_hash_looktype(name: &str) -> u16 {
    let h: u32 = name.bytes().fold(0u32, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u32::from(b))
    });
    20 + (h % 480) as u16 // 20..=499 covers a wide range of creature sprites
}
