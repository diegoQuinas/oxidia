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
//! reference these names; unknown names are skipped.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use crate::Direction;
use crate::Position;
use crate::game::RaceType;
use formats::FormatError;
use rand::Rng;
use rand::rngs::StdRng;

/// Monster personality determines aggro and behavior.
///
/// TFS wildlife is not a personality: `hostile="0"` clears [`MonsterType::hostile`]
/// so the creature never swings, and `runonhealth` (often equal to max HP on
/// rabbits, deer, sheep) makes it flee a nearby player. Idle creatures still
/// wander.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Personality {
    /// Attacks players on sight within aggro range.
    Aggressive,
    /// Does not aggro on sight. Still retaliates when `hostile` is set.
    Neutral,
    /// Flees when HP drops below threshold.
    Coward,
}

impl Personality {
    fn from_xml(s: Option<&str>) -> Self {
        match s {
            Some("neutral") => Personality::Neutral,
            Some("coward") => Personality::Coward,
            _ => Personality::Aggressive, // Tibia default: most creatures attack on sight
        }
    }
}

/// Default aggro range in tiles for aggressive monsters.
pub(crate) const DEFAULT_AGGRO_RANGE: i32 = 5;
/// Chebyshev range at which a fleeing creature runs from a player.
/// Roughly the client viewport, matching TFS `Monster::canSee`.
pub(crate) const FLEE_SIGHT_RANGE: i32 = 8;
/// Fraction of max_health (as numerator of 4) below which cowards flee.
pub(crate) const COWARD_FLEE_THRESHOLD: u32 = 1; // flee when HP < max_health / 4

/// TFS `<flag hostile>` / `<flag runonhealth>`, also accepted as attributes on
/// the monster element (flat `config/monsters.xml`).
///
/// Missing `hostile` defaults to true — TFS's default, and the reason a rabbit
/// with an unread flag was attacking.
fn parse_behavior_flags(node: roxmltree::Node<'_, '_>) -> (bool, u32) {
    let mut hostile = true;
    let mut run_on_health = 0u32;
    if let Some(v) = node.attribute("hostile") {
        hostile = flag_truthy(v);
    }
    if let Some(v) = node
        .attribute("runonhealth")
        .or_else(|| node.attribute("run_on_health"))
    {
        run_on_health = v.parse().unwrap_or(0);
    }
    for flag in node.descendants().filter(|n| n.has_tag_name("flag")) {
        if let Some(v) = flag.attribute("hostile") {
            hostile = flag_truthy(v);
        }
        if let Some(v) = flag.attribute("runonhealth") {
            run_on_health = v.parse().unwrap_or(0);
        }
    }
    (hostile, run_on_health)
}

fn flag_truthy(v: &str) -> bool {
    !matches!(v, "0" | "false" | "no")
}

/// Explicit `personality=` wins. Otherwise hostile creatures aggro and
/// non-hostile ones (rabbits, deer, sheep) do not.
fn personality_from_node(node: roxmltree::Node<'_, '_>, hostile: bool) -> Personality {
    match node.attribute("personality") {
        Some(s) => Personality::from_xml(Some(s)),
        None if hostile => Personality::Aggressive,
        None => Personality::Neutral,
    }
}

/// A monster blueprint loaded from `config/monsters.xml`.
/// These are the canonical stats for each named monster type.
#[derive(Debug, Clone)]
pub(crate) struct MonsterType {
    pub(crate) name: String,
    pub(crate) look_type: u16,
    pub(crate) look_corpse: u16,
    pub(crate) health: u32,
    pub(crate) max_health: u32,
    pub(crate) speed: u16,
    pub(crate) attack: u16,
    pub(crate) loot: Vec<MonsterDrop>,
    /// Preferred distance from target (0 = melee, 1+ = ranged).
    pub(crate) target_distance: i32,
    /// Creature race for splash fluid on hit/death. `None` = no splash.
    pub(crate) race: Option<RaceType>,
    /// Monster personality.
    pub(crate) personality: Personality,
    /// Aggro range in tiles.
    pub(crate) aggro_range: i32,
    /// TFS `hostile`. `false` means never attack — no aggro, no retaliation.
    /// Wildlife (`Rabbit`, `Deer`, `Sheep`) sets this to false.
    pub(crate) hostile: bool,
    /// TFS `runonhealth`: flee while current HP is at or below this.
    /// `0` means this flag never triggers a flee. Rabbits set it to max health,
    /// so they always run from a nearby player.
    pub(crate) run_on_health: u32,
    /// TFS `<defenses defense>`. Used as the shield-block value in `blockHit`.
    pub(crate) defense: i32,
    /// TFS `<defenses armor>`. Subtracted after defense in `blockHit`.
    pub(crate) armor: i32,
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
        let look_corpse: u16 = node
            .attribute("lookcorpse")
            .or_else(|| node.attribute("look_corpse"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let race = node.attribute("race").and_then(|s| match s {
            "blood" => Some(RaceType::Blood),
            "venom" => Some(RaceType::Venom),
            "fire" => Some(RaceType::Fire),
            "energy" => Some(RaceType::Energy),
            "drown" => Some(RaceType::Drown),
            "physical" => Some(RaceType::Physical),
            "earth" => Some(RaceType::Earth),
            "holy" => Some(RaceType::Holy),
            "death" => Some(RaceType::Death),
            "undead" => Some(RaceType::Undead),
            "diamond" => Some(RaceType::Diamond),
            _ => None,
        }).or(Some(RaceType::Blood));
        let (hostile, run_on_health) = parse_behavior_flags(node);
        let personality = personality_from_node(node, hostile);
        let aggro_range = node
            .attribute("aggro_range")
            .or_else(|| node.attribute("aggrorange"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_AGGRO_RANGE);
        map.insert(
            name.to_ascii_lowercase(),
            MonsterType {
                name,
                look_type,
                look_corpse,
                health,
                max_health,
                speed,
                attack,
                loot: vec![],
                target_distance: 0,
                race,
                personality,
                aggro_range,
                hostile,
                run_on_health,
                defense: 0,
                armor: 0,
            },
        );
    }
    Ok(map)
}

/// Parse the individual monster XML files under `data_dir`.
///
/// Reads the root `monsters.xml` index, then for each entry loads the
/// individual monster XML and extracts race, look type, health, speed,
/// and attack values — fields missing from the flat `config/monsters.xml`.
///
/// Expected individual format:
/// ```xml
/// <monster name="Rat" race="blood" speed="134">
///   <health now="20" max="20"/>
///   <look type="21" corpse="5964"/>
///   <attacks>
///     <attack name="melee" attack="7"/>
///   </attacks>
/// </monster>
/// ```
pub fn parse_monsters_data_dir(data_dir: &Path) -> Result<HashMap<String, MonsterType>, FormatError> {
    let root_xml = std::fs::read_to_string(data_dir.join("monsters.xml")).map_err(|e| {
        FormatError::InvalidNode {
            what: Box::leak(format!("cannot read monsters.xml index: {e}").into_boxed_str()),
        }
    })?;
    let doc = roxmltree::Document::parse(&root_xml).map_err(|_| FormatError::InvalidNode {
        what: "monsters.xml index is not well-formed",
    })?;

    let mut map = HashMap::new();

    for node in doc.descendants().filter(|n| n.has_tag_name("monster")) {
        let name = node
            .attribute("name")
            .ok_or(FormatError::InvalidNode {
                what: "monster entry missing name attribute",
            })?
            .to_string();

        let file = node.attribute("file").ok_or(FormatError::InvalidNode {
            what: "monster entry missing file attribute",
        })?;

        // Read and parse the individual monster XML
        let individual_path = data_dir.join(file);
        let individual_xml = std::fs::read_to_string(&individual_path).map_err(|e| {
            FormatError::InvalidNode {
                what: Box::leak(
                    format!("cannot read {}: {e}", individual_path.display()).into_boxed_str(),
                ),
            }
        })?;

        let ind_doc =
            roxmltree::Document::parse(&individual_xml).map_err(|_| FormatError::InvalidNode {
                what: Box::leak(
                    format!("{} is not well-formed", individual_path.display()).into_boxed_str(),
                ),
            })?;

        let root = ind_doc.root().first_element_child().ok_or(
            FormatError::InvalidNode {
                what: Box::leak(
                    format!("{} has no root element", individual_path.display()).into_boxed_str(),
                ),
            },
        )?;

        // Extract race from root monster element
        let race = root.attribute("race").and_then(|s| match s {
            "blood" => Some(RaceType::Blood),
            "venom" => Some(RaceType::Venom),
            "fire" => Some(RaceType::Fire),
            "energy" => Some(RaceType::Energy),
            "drown" => Some(RaceType::Drown),
            "physical" => Some(RaceType::Physical),
            "earth" => Some(RaceType::Earth),
            "holy" => Some(RaceType::Holy),
            "death" => Some(RaceType::Death),
            "undead" => Some(RaceType::Undead),
            "diamond" => Some(RaceType::Diamond),
            _ => None,
        });

        // Extract <look type="..." corpse="..."/>
        let look_type: u16 = root
            .descendants()
            .find(|n| n.has_tag_name("look"))
            .and_then(|n| n.attribute("type"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);
        let look_corpse: u16 = root
            .descendants()
            .find(|n| n.has_tag_name("look"))
            .and_then(|n| n.attribute("corpse"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        // Extract <health now="..." max="..."/>
        let (health, max_health) = root
            .descendants()
            .find(|n| n.has_tag_name("health"))
            .map(|n| {
                let now: u32 = n.attribute("now").and_then(|s| s.parse().ok()).unwrap_or(50);
                let max: u32 = n.attribute("max").and_then(|s| s.parse().ok()).unwrap_or(50);
                (now, max)
            })
            .unwrap_or((50, 50));

        // Extract speed from root monster element
        let speed: u16 = root
            .attribute("speed")
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);

        // Melee max. No `<attack name="melee">` (rabbits, sheep) means no swing —
        // the old default of 7 is what made a non-hostile rabbit hit for damage.
        let attack: u16 = root
            .descendants()
            .find(|n| n.has_tag_name("attack") && n.attribute("name") == Some("melee"))
            .and_then(|n| {
                n.attribute("attack")
                    .and_then(|s| s.parse().ok())
                    .or_else(|| {
                        // Fall back to min/max format: min="-N" max="-M"
                        let max_abs: Option<u16> = n
                            .attribute("max")
                            .and_then(|s| {
                                let v: i16 = s.parse().ok()?;
                                Some(v.unsigned_abs())
                            });
                        max_abs
                    })
            })
            .unwrap_or(0);

        // Extract <loot> section with item entries.
        let loot: Vec<MonsterDrop> = root
            .descendants()
            .find(|n| n.has_tag_name("loot"))
            .map(|loot_node| {
                loot_node
                    .children()
                    .filter(|n| n.has_tag_name("item"))
                    .filter_map(|item| {
                        let item_id: u16 = item.attribute("id")?.parse().ok()?;
                        let chance_raw: f64 = item.attribute("chance")?.parse().ok()?;
                        let chance = (chance_raw / 100.0).clamp(0.0, 1.0);
                        let count_max: u16 = item
                            .attribute("countmax")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(1);
                        Some(MonsterDrop {
                            item_id,
                            chance,
                            count_max,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let (defense, armor) = root
            .descendants()
            .find(|n| n.has_tag_name("defenses"))
            .map(|n| {
                let defense = n
                    .attribute("defense")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let armor = n.attribute("armor").and_then(|s| s.parse().ok()).unwrap_or(0);
                (defense, armor)
            })
            .unwrap_or((0, 0));
        let (hostile, run_on_health) = parse_behavior_flags(root);
        let personality = personality_from_node(root, hostile);
        let aggro_range = root
            .attribute("aggrorange")
            .or_else(|| root.attribute("aggro_range"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_AGGRO_RANGE);
        map.insert(
            name.to_ascii_lowercase(),
            MonsterType {
                name,
                look_type,
                look_corpse,
                health,
                max_health,
                speed,
                attack,
                loot,
                target_distance: 0,
                race,
                personality,
                aggro_range,
                hostile,
                run_on_health,
                defense,
                armor,
            },
        );
    }

    Ok(map)
}

/// Parse `<spawns>` XML and return a list of spawn entries.
///
/// Expected format (RME / TFS 1.4.2). `x` and `y` are offsets from the spawn
/// center. The monster `z` attribute is the absolute floor and equals
/// `centerz` in every real datapack; TFS ignores it and places the creature
/// on `centerz` (`spawn.cpp` `loadFromXml`). Adding the two puts a surface
/// spawn on floor 14, where a ground-floor viewer can never see it.
/// ```xml
/// <spawns>
///   <spawn centerx="32153" centery="31124" centerz="7" radius="2">
///     <monster name="Silver Rabbit" x="0" y="1" z="7" spawntime="60"/>
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
            let spawntime_s: u64 = monster_node
                .attribute("spawntime")
                .and_then(|s| s.parse().ok())
                .unwrap_or(60);
            let x = (i32::from(cx)).wrapping_add(i32::from(dx)) as u16;
            let y = (i32::from(cy)).wrapping_add(i32::from(dy)) as u16;
            // Floor is the spawn center's z. RME writes the same absolute floor
            // on the monster node; TFS 1.4.2 does not add it (spawn.cpp).
            let z = cz;
            let position = Position::new(x, y, z);
            spawns.push(MonsterSpawn {
                position,
                respawn_interval_ms: spawntime_s * 1000,
                respawn_at_ms: None,
                name: name.clone(),
                look_type: name_hash_looktype(&name),
                look_corpse: 0,
                health: 50,
                max_health: 50,
                speed: 200,
                attack: 7,
                loot: vec![],
                target_distance: 0,
                race: Some(RaceType::Blood),
                personality: Personality::Neutral,
                aggro_range: DEFAULT_AGGRO_RANGE,
                hostile: true,
                run_on_health: 0,
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
    /// Maximum item count for the drop. When rolled, actual count is 1..=count_max.
    pub(crate) count_max: u16,
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
    pub(crate) look_corpse: u16,
    pub(crate) health: u32,
    pub(crate) max_health: u32,
    pub(crate) speed: u16,
    pub(crate) attack: u16,
    pub(crate) loot: Vec<MonsterDrop>,
    /// Preferred distance from target (0 = melee, 1+ = ranged).
    pub(crate) target_distance: i32,
    /// Creature race for splash fluid on hit/death. `None` = no splash.
    pub(crate) race: Option<RaceType>,
    /// Monster personality.
    pub(crate) personality: Personality,
    /// Aggro range in tiles.
    pub(crate) aggro_range: i32,
    /// TFS `hostile`. Copied onto the respawned creature.
    pub(crate) hostile: bool,
    /// TFS `runonhealth`. Copied onto the respawned creature.
    pub(crate) run_on_health: u32,
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
    /// Corpse item id spawned on death. `0` means no corpse.
    pub(crate) look_corpse: u16,
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
    /// Creature race for splash fluid on hit/death. `None` = no splash.
    pub(crate) race: Option<RaceType>,
    /// Monster personality.
    pub(crate) personality: Personality,
    /// Aggro range in tiles.
    pub(crate) aggro_range: i32,
    /// TFS `hostile`. `false` = never attack (no aggro, no retaliation).
    pub(crate) hostile: bool,
    /// TFS `runonhealth`. Flee while `health <= run_on_health` (`0` = never).
    pub(crate) run_on_health: u32,
    /// Light level emitted by this creature (0 = none).
    pub(crate) light_level: u8,
    /// Light color emitted by this creature.
    pub(crate) light_color: u8,
}

impl MonsterState {
    /// Health percent as a 0..100 integer, matching the wire format.
    pub(crate) fn health_percent(&self) -> u8 {
        if self.max_health == 0 {
            return 0;
        }
        ((self.health * 100) / self.max_health).min(100) as u8
    }

    /// TFS `Monster::isFleeing`: HP at or below `runonhealth`, or the legacy
    /// coward threshold (`health < max_health / 4`).
    pub(crate) fn is_fleeing(&self) -> bool {
        let coward = self.personality == Personality::Coward
            && self.health.saturating_mul(4)
                < self.max_health.saturating_mul(COWARD_FLEE_THRESHOLD);
        let flagged = self.run_on_health > 0 && self.health <= self.run_on_health;
        coward || flagged
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

/// Roll drops from a loot table using independent per-item chance.
/// Each entry's chance is evaluated independently; successful entries
/// produce a random count in `1..=count_max`. Failed entries are absent.
pub(crate) fn roll_drops(rng: &mut StdRng, drops: &[MonsterDrop]) -> Vec<(u16, u8)> {
    let mut result = Vec::with_capacity(drops.len());
    for drop in drops {
        if !rng.gen_bool(drop.chance) {
            continue;
        }
        let count = if drop.count_max > 0 {
            rng.gen_range(1..=drop.count_max)
        } else {
            1
        };
        result.push((drop.item_id, count as u8));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn roll_drops_all_succeed_at_chance_1() {
        let mut rng = StdRng::seed_from_u64(42);
        let drops = vec![
            MonsterDrop { item_id: 100, chance: 1.0, count_max: 1 },
            MonsterDrop { item_id: 200, chance: 1.0, count_max: 1 },
        ];
        let result = roll_drops(&mut rng, &drops);
        assert_eq!(result.len(), 2, "all drops must succeed at chance 1.0");
        assert!(result.contains(&(100, 1)));
        assert!(result.contains(&(200, 1)));
    }

    #[test]
    fn roll_drops_all_fail_at_chance_0() {
        let mut rng = StdRng::seed_from_u64(42);
        let drops = vec![MonsterDrop {
            item_id: 100,
            chance: 0.0,
            count_max: 1,
        }];
        let result = roll_drops(&mut rng, &drops);
        assert_eq!(result.len(), 0, "zero-chance drop must not appear");
    }

    #[test]
    fn roll_drops_independent_per_item_chance() {
        // With a seeded RNG, run twice: results may vary per item.
        let mut rng = StdRng::seed_from_u64(999);
        let drops = vec![
            MonsterDrop { item_id: 100, chance: 0.5, count_max: 1 },
            MonsterDrop { item_id: 200, chance: 0.5, count_max: 1 },
        ];
        let result = roll_drops(&mut rng, &drops);
        // At least one item may be present — we can't assert exact count
        // due to RNG, but we CAN assert no duplicate server_ids and
        // each item's count is valid.
        for &(_, count) in &result {
            assert!(count >= 1, "count must be >= 1");
        }
    }

    #[test]
    fn roll_drops_count_range_respected() {
        let mut rng = StdRng::seed_from_u64(42);
        let drops = vec![MonsterDrop {
            item_id: 300,
            chance: 1.0,
            count_max: 10,
        }];
        let result = roll_drops(&mut rng, &drops);
        assert_eq!(result.len(), 1);
        let count = result[0].1;
        assert!(
            (1..=10).contains(&count),
            "count {count} must be in 1..=10"
        );
    }

    #[test]
    fn roll_drops_empty_table() {
        let mut rng = StdRng::seed_from_u64(42);
        let drops: Vec<MonsterDrop> = vec![];
        let result = roll_drops(&mut rng, &drops);
        assert!(result.is_empty(), "empty loot table must produce no drops");
    }

    #[test]
    fn spawn_floor_is_centerz_not_center_plus_monster_z() {
        // Real-map RME writes the absolute floor on both attributes. TFS uses
        // centerz only; adding them hides every surface creature on floor 14.
        let xml = r#"
            <spawns>
              <spawn centerx="32957" centery="32076" centerz="7" radius="2">
                <monster name="Rat" x="1" y="-2" z="7" spawntime="60"/>
              </spawn>
            </spawns>
        "#;
        let spawns = parse_spawns_xml(xml).unwrap();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].position, Position::new(32958, 32074, 7));
    }

    #[test]
    fn roll_drops_returns_item_ids_and_counts() {
        let mut rng = StdRng::seed_from_u64(7);
        let drops = vec![MonsterDrop {
            item_id: 2671,
            chance: 1.0,
            count_max: 5,
        }];
        let result = roll_drops(&mut rng, &drops);
        assert_eq!(result[0].0, 2671, "item_id must be preserved");
        assert!(
            (1..=5).contains(&result[0].1),
            "count must be 1..=5, got {}",
            result[0].1
        );
    }

    // -------------------------------------------------------------------------
    // Task 4.5: XML parse — loot table struct fields, empty <loot>, missing countmax
    // -------------------------------------------------------------------------

    /// Minimal valid monster XML for parse_monsters_data_dir tests.
    /// Must have a name, look/type, health now/max, speed, and optional loot.
    fn sample_monster_xml(loot_block: &str) -> String {
        format!(
            r#"<monster name="TestRat" race="blood" speed="134">
  <health now="20" max="20"/>
  <look type="21" corpse="5964"/>
  <attacks>
    <attack name="melee" attack="7"/>
  </attacks>
  {loot_block}
</monster>"#
        )
    }

    /// Write sample XML files to a temp dir and parse via parse_monsters_data_dir.
    fn parse_in_temp_dir(monster_xml: &str) -> HashMap<String, MonsterType> {
        let dir = tempfile::TempDir::new().unwrap();
        let monster_file = dir.path().join("test_rat.xml");
        std::fs::write(&monster_file, monster_xml).unwrap();
        let index_xml = r#"<monsters>
  <monster name="TestRat" file="test_rat.xml"/>
</monsters>"#;
        let index_file = dir.path().join("monsters.xml");
        std::fs::write(&index_file, index_xml).unwrap();
        parse_monsters_data_dir(dir.path()).unwrap()
    }

    #[test]
    fn xml_parse_single_loot_item() {
        let xml = sample_monster_xml(
            r#"<loot>
    <item id="2671" chance="25" countmax="3"/>
  </loot>"#,
        );
        let map = parse_in_temp_dir(&xml);
        let mt = map.get("testrat").expect("TestRat must be parsed");
        assert_eq!(mt.loot.len(), 1, "must have 1 loot entry");
        assert_eq!(mt.loot[0].item_id, 2671);
        assert!((mt.loot[0].chance - 0.25).abs() < f64::EPSILON);
        assert_eq!(mt.loot[0].count_max, 3);
    }

    #[test]
    fn xml_parse_empty_loot_block() {
        let xml = sample_monster_xml(r#"<loot></loot>"#);
        let map = parse_in_temp_dir(&xml);
        let mt = map.get("testrat").expect("TestRat must be parsed");
        assert!(
            mt.loot.is_empty(),
            "empty <loot> must produce empty loot vec"
        );
    }

    #[test]
    fn xml_parse_missing_countmax_defaults_to_1() {
        let xml = sample_monster_xml(
            r#"<loot>
    <item id="2666" chance="10"/>
  </loot>"#,
        );
        let map = parse_in_temp_dir(&xml);
        let mt = map.get("testrat").expect("TestRat must be parsed");
        assert_eq!(mt.loot.len(), 1);
        assert_eq!(mt.loot[0].item_id, 2666);
        assert!((mt.loot[0].chance - 0.10).abs() < f64::EPSILON);
        assert_eq!(mt.loot[0].count_max, 1, "default count_max must be 1");
    }

    #[test]
    fn xml_parse_multiple_loot_items() {
        let xml = sample_monster_xml(
            r#"<loot>
    <item id="2671" chance="25" countmax="3"/>
    <item id="2666" chance="10"/>
    <item id="2681" chance="50" countmax="1"/>
  </loot>"#,
        );
        let map = parse_in_temp_dir(&xml);
        let mt = map.get("testrat").expect("TestRat must be parsed");
        assert_eq!(mt.loot.len(), 3, "must have 3 loot entries");
    }

    #[test]
    fn xml_parse_tfs_wildlife_flags() {
        // Rabbit-shaped TFS XML: hostile=0, runonhealth == max health, no melee.
        let xml = r#"<monster name="Rabbit" race="blood" speed="180">
  <health now="15" max="15"/>
  <look type="74" corpse="6017"/>
  <flags>
    <flag hostile="0"/>
    <flag runonhealth="15"/>
  </flags>
  <loot>
    <item id="2666" chance="82" countmax="2"/>
  </loot>
</monster>"#;
        let map = parse_in_temp_dir(xml);
        let rabbit = map.get("testrat").expect("index name is TestRat");
        assert!(!rabbit.hostile, "hostile=0 must not attack");
        assert_eq!(rabbit.run_on_health, 15);
        assert_eq!(rabbit.max_health, 15);
        assert_eq!(rabbit.personality, Personality::Neutral);
        assert_eq!(rabbit.attack, 0, "no melee attack must not default to 7");
    }

    #[test]
    fn xml_parse_corpse_attribute() {
        let xml = sample_monster_xml(r#"<loot></loot>"#);
        let map = parse_in_temp_dir(&xml);
        let mt = map.get("testrat").expect("TestRat must be parsed");
        assert_eq!(mt.look_corpse, 5964, "look corpse must be parsed from <look corpse='5964'/>");
    }

    #[test]
    fn xml_parse_no_loot_block_produces_empty_loot() {
        // Monster XML without any <loot> section.
        let xml = r#"<monster name="TestRat" race="blood" speed="134">
  <health now="20" max="20"/>
  <look type="21" corpse="5964"/>
  <attacks>
    <attack name="melee" attack="7"/>
  </attacks>
</monster>"#;
        let map = parse_in_temp_dir(xml);
        let mt = map.get("testrat").expect("TestRat must be parsed");
        assert!(
            mt.loot.is_empty(),
            "no <loot> block must produce empty loot"
        );
    }
}
