#![forbid(unsafe_code)]
//! The authoritative game loop — M5 unified-push actor.
//!
//! Each session owns an `mpsc<Vec<u8>>` whose `Sender` lives in the actor.
//! The actor is the single builder of all outbound packets, computes spectators,
//! owns the known-creature set, and broadcasts presence events (login appear,
//! walk move/appear/remove, turn, logout remove).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use rand::{SeedableRng, rngs::StdRng};

use protocol::chat::SpeakType;
use protocol::combat_packets;
use protocol::creature::{self, CreatureView, Outfit};
use protocol::map_description::{PlacedCreature, WireItem};
use protocol::outfit as outfit_packets;
use protocol::{enter_world, tile_creature, tile_item, walk};

use crate::map::ChunkId;
use crate::map::{ChunkManager, WorldMeta};
use crate::pathfinding;
use crate::{Direction, Position};

use self::condition::ConditionRegeneration;
use self::lua::LuaRuntime;
pub(super) use self::monster::{
    MonsterDrop, MonsterSpawn, MonsterState, MonsterType, name_hash_looktype,
    parse_monsters_data_dir, parse_monsters_xml, parse_spawns_xml,
};
use self::xml_registry::XmlRegistry;

mod chat;
mod combat;
mod condition;
mod containers;
mod gm;
mod items;
mod look;
mod lua;
mod monster;
mod movement;
mod session;
#[cfg(test)]
mod test_support;
mod xml_registry;

/// Outbound channel depth per session. A client that backs this up past the cap
/// is treated as dead (logged out) rather than blocking the game loop or growing
/// memory unbounded.
const PUSH_CAPACITY: usize = 256;

/// Attack interval for the no-vocation fist melee, matching TFS vocations.xml
/// "None" attackspeed (`player.cpp:351-358`).
pub const MELEE_ATTACK_INTERVAL_MS: u64 = 2000;

/// How often the global combat tick fires. Finer than the attack interval so
/// timing granularity is good; cheap enough that the actor is not taxed.
const COMBAT_TICK_MS: u64 = 250;

/// How often one bucket of the monster AI fires (10 buckets × 100ms = 1s cycle).
const MONSTER_AI_TICK_MS: u64 = 100;

/// How often the regeneration tick fires. Matches TFS
/// `Condition::executeConditions` interval (1s).
const REGENERATION_TICK_MS: u64 = 1000;

/// TFS `MESSAGE_STATUS_SMALL = 21` (`const.h:190`): white status-bar message.
/// Used for PZ-rejection ("You may not attack…").
const MSG_STATUS_SMALL: u8 = 21;

/// Minimum step interval in ms (200ms = 5 steps/s ceiling).
const MIN_STEP_MS: u64 = 200;
/// Maximum step interval in ms (2000ms = 0.5 steps/s floor).
const MAX_STEP_MS: u64 = 2000;

/// Compute the step interval in milliseconds for a given creature speed.
/// Matches the TFS inverse relationship: higher speed → shorter step time.
/// Default player speed (220) yields ~454ms → ~2.2 steps/s.
fn step_time(speed: u16) -> u64 {
    (110_000 / speed.max(1) as u64).clamp(MIN_STEP_MS, MAX_STEP_MS)
}

/// TFS `MESSAGE_INFO_DESCR = 22`: green look-description message (`const.h:191`).
const MSG_INFO_DESCR: u8 = 22;

/// TFS `MESSAGE_STATUS_CONSOLE_BLUE = 4` (`const.h:182`): blue console text.
/// Used for GM command output (`/help`).
const MSG_CONSOLE_BLUE: u8 = 4;

/// Ghost sprite looktype used when GM `/ghost` mode is active.
/// Verify against client `.dat` if visual mismatch occurs.
const GHOST_LOOKTYPE: u16 = 40;

/// Creature race determining splash fluid type on combat hit/death.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RaceType {
    Blood,
    Venom,
    Fire,
    Energy,
    Drown,
    Physical,
    Earth,
    Holy,
    Death,
    Undead,
    Diamond,
}

impl RaceType {
    /// Fluid subtype byte for splash items, or `None` for races that produce no splash.
    pub(crate) fn fluid_subtype(self) -> Option<u8> {
        match self {
            Self::Blood => Some(5),   // FLUID_BLOOD (OTClient: 5 = red)
            Self::Venom => Some(6),   // FLUID_SLIME (OTClient: 6 = green)
            _ => None,                // no splash
        }
    }
}

/// Client sprite id for the small (on-hit) splash item.
/// items.otb: server_id=2019, client_id=2889, group=11 (SPLASH).
const ITEM_SMALLSPLASH: u16 = 2889;

/// Client sprite id for the full (on-death) splash item.
/// items.otb: server_id=2016, client_id=2886, group=11 (SPLASH).
const ITEM_FULLSPLASH: u16 = 2886;

/// Client viewport extents from the player's tile, matching the 18x14 map
/// description anchored at center-8 / center-6 (TFS `Map::maxClientViewportX/Y`).
/// Asymmetric: one extra column east and one extra row south.
const VIEW_LEFT: i32 = 8; // columns west of center
const VIEW_RIGHT: i32 = 9; // columns east of center (the +1 edge)
const VIEW_UP: i32 = 6; // rows north of center
const VIEW_DOWN: i32 = 7; // rows south of center (the +1 edge)

/// What the game service needs to build the enter-world burst for a player.
#[derive(Debug, Clone, Copy)]
pub struct PlayerSnapshot {
    pub id: u32,
    pub position: Position,
    pub direction: Direction,
    /// The outfit the player logged in with (restored or default).
    pub outfit: Outfit,
    /// Current hit points at login (restored or default 150).
    pub health: u32,
    /// Maximum hit points at login (restored or default 150).
    pub max_health: u32,
}

/// Login result: the new player's snapshot plus the already-in-range players,
/// pre-serialized as creature things to splice into the enter-world map.
pub struct LoginAck {
    pub snapshot: PlayerSnapshot,
    pub others: Vec<PlacedCreature>,
    /// Pre-encoded enter-world map slice, built from the MERGED view (static map
    /// plus the dynamic overlay) so a returning player sees items dropped on the
    /// ground by others. The server layer splices these bytes verbatim into the
    /// burst instead of re-encoding from the pristine `StaticMap`.
    pub map_description: Vec<u8>,
}

/// Initial state supplied to `Game::login`. When a `PlayerSave` exists for the
/// character, the server layer maps it into this struct; otherwise it provides
/// defaults (position `None` → `free_spawn()`, default outfit/health).
pub struct InitialState {
    /// Saved position, or `None` to fall back to `free_spawn()`.
    pub position: Option<Position>,
    /// Facing direction at login.
    pub direction: Direction,
    /// Visual outfit at login.
    pub outfit: Outfit,
    /// Current hit points.
    pub health: u32,
    /// Maximum hit points.
    pub max_health: u32,
    /// Character sex: 0 = female, 1 = male (TFS outfits.xml `type` convention).
    /// Selects the gendered outfit catalog served by do_request_outfit.
    pub sex: u8,
    /// `true` if the session authenticated as a gamemaster (look-at debug info).
    pub gamemaster: bool,
    /// Equipped items to restore: `(slot 1..=10, server_id, count)`. Empty for
    /// new characters. The world resolves each `server_id` via the item catalog.
    pub inventory: Vec<(u8, u16, u8)>,
    /// Container contents to restore: `(inv_slot 1..=10, path, server_id, count)`.
    /// `path` is `""` for items directly in the top-level bag, `"N"` for items
    /// inside the sub-container at slot N of that bag, and so on for deeper nesting.
    pub container_items: Vec<(u8, String, u16, u8)>,
}

/// Emitted on the save channel the instant a player leaves the game.
/// The server worker maps this into `persistence::PlayerSave` and awaits
/// `store.save_player`. The world crate does NOT depend on `persistence`.
#[derive(Debug, Clone)]
pub struct SaveRecord {
    pub name: String,
    pub position: Position,
    pub direction: Direction,
    pub outfit: Outfit,
    pub health: u32,
    pub max_health: u32,
    pub sex: u8,
    /// Equipped items at logout: `(slot 1..=10, server_id, count)`.
    pub inventory: Vec<(u8, u16, u8)>,
    /// Container contents at logout: `(inv_slot 1..=10, path, server_id, count)`.
    pub container_items: Vec<(u8, String, u16, u8)>,
}

/// One equipped item, with the cached wire fields needed to re-send `0x78`.
#[derive(Debug, Clone, Copy)]
struct InvItem {
    server_id: u16,
    client_id: u16,
    /// Stack count for stackables (ammo); `None` for non-stackables.
    count: Option<u8>,
    animated: bool,
}

/// One item held inside an open container.
#[derive(Debug, Clone, Copy)]
struct ContainerItem {
    server_id: u16,
    client_id: u16,
    count: Option<u8>,
    animated: bool,
}

impl ContainerItem {
    fn wire(&self) -> protocol::container::ContainerWireItem {
        protocol::container::ContainerWireItem {
            client_id: self.client_id,
            subtype: self.count,
            animated: self.animated,
        }
    }
}

/// Where a container was opened from (determines `has_parent` and navigation).
#[derive(Debug, Clone, Copy)]
enum ContainerSource {
    /// Opened from inventory slot 1..=10.
    Slot(u8),
    /// Opened from inside another open container (parent cid + item slot).
    Nested { parent_cid: u8, parent_slot: u8 },
    /// Opened from a container lying on the ground, at the given tile. Not
    /// persisted; auto-closes when the player walks out of range (TFS).
    Ground(Position),
}

/// One container the player has in their possession, with an optional open window.
/// Contents survive close+reopen within the same session (`is_open` toggles the
/// visibility; `= None` in the cid slot means the slot itself is unallocated).
#[derive(Debug, Clone)]
struct OpenContainer {
    #[allow(dead_code)] // retained for future lookup-by-item-type use
    server_id: u16,
    client_id: u16,
    capacity: u8,
    name: String,
    items: Vec<ContainerItem>,
    source: ContainerSource,
    /// Whether the client window is currently showing. `false` means the player
    /// closed the window but the items are still in memory.
    is_open: bool,
}

struct PlayerState {
    name: String,
    position: Position,
    direction: Direction,
    outfit: Outfit,
    push_tx: mpsc::Sender<Vec<u8>>,
    known: HashSet<u32>,
    // --- M7 combat fields ---
    /// Current hit points.
    health: u32,
    /// Maximum hit points (TFS default for a new character = 150).
    max_health: u32,
    /// Fist-skill level (TFS default = 10).
    fist_skill: i32,
    /// Id of the current attack target (`None` = not fighting).
    attacking: Option<u32>,
    /// Timestamp of the last swing, in the same monotonic-ms space as
    /// `CombatTick { now_ms }`. Initialized to 0 so the first eligible tick
    /// swings immediately (mirrors TFS `doAttacking` priming logic).
    last_attack_ms: u64,
    /// Character sex: 0 = female, 1 = male (TFS outfits.xml `type` convention).
    /// Determines which gendered outfit catalog is sent in the 0xC8 window.
    sex: u8,
    /// Gamemaster flag from login; gates look-at debug (item id + position).
    gamemaster: bool,
    /// Creature race — defaults to Blood for players (always produces blood splash).
    race: RaceType,
    /// Ghost mode: GM invisible to non-GMs, bypasses collision.
    /// Runtime-only — reset on login/logout.
    ghost: bool,
    /// Previous outfit before ghost mode was toggled on.
    /// `None` when ghost mode is off.
    prev_outfit: Option<Outfit>,
    /// Noclip mode: bypasses collision without visibility changes.
    /// Runtime-only — reset on login/logout.
    noclip: bool,
    /// Movement speed override. Default 220 on login. Runtime-only — not
    /// persisted; reset on login/logout.
    speed: u16,
    /// Equipment slots 1..=10, indexed 0..=9. `None` = empty slot.
    inventory: [Option<InvItem>; 10],
    /// Open container windows, indexed by cid (0..=15). `None` = window not open.
    open_containers: [Option<OpenContainer>; 16],
    /// Creature id this player is auto-following (0xA2 follow). `None` when idle.
    follow_target: Option<u32>,
    /// Target position for click-to-move auto-walk (0x64 GoTo). `None` when idle.
    /// Cleared on arrival, manual move, PZ entry, ESC, or path-failure.
    /// Mutually exclusive with `follow_target` — setting one clears the other.
    go_to_position: Option<Position>,
    /// Consecutive failed repath attempts for go_to_position.
    /// Reset on successful step; cleared with go_to_position.
    failed_repaths: Option<u32>,
    /// Enqueued walk directions consumed one per auto-walk tick.
    /// Filled by `get_path_matching` when the player has a follow target.
    list_walk_dir: VecDeque<Direction>,
    /// Monotonic-ms timestamp of the last successful auto-walk step.
    /// Used to gate steps by `step_time()` so the player moves at their
    /// speed-appropriate pace rather than 100ms AI-tick rate.
    last_walk_ms: u64,
    /// Active timed conditions (regeneration, etc.). Extended when the
    /// player eats more food; ticked by `RegenerationTick`.
    conditions: Vec<ConditionRegeneration>,
}

struct Game {
    chunks: ChunkManager,
    /// Shared world metadata (spawn, towns, item catalogue). Owned by the actor
    /// and shared with WorldHandle via Arc.
    meta: Arc<WorldMeta>,
    /// Copy-on-write overlay of runtime-modified tile stacks (M10.1). Empty until
    /// the first item move; reads fall back to `map` for untouched tiles.
    dynamic: std::collections::HashMap<(u16, u16, u8), crate::map::TileStack>,
    players: HashMap<u32, PlayerState>,
    monsters: HashMap<u32, MonsterState>,
    next_id: u32,
    #[allow(dead_code)]
    next_monster_id: u32,
    /// Auto-incrementing id for spawn entries.
    #[allow(dead_code)]
    next_spawn_id: u32,
    /// Blueprints for monster respawns, keyed by spawn id.
    spawns: HashMap<u32, MonsterSpawn>,
    next_statement_id: u32,
    /// RNG for combat damage rolls. A single actor-owned RNG keeps the loop
    /// lock-free (no shared state) and is seedable in tests for determinism.
    rng: StdRng,
    /// Channel to the background save worker. `None` in unit tests and until
    /// `spawn()` wires it in. Unbounded so `logout` never blocks the actor.
    save_tx: Option<mpsc::UnboundedSender<SaveRecord>>,
    /// Lua scripting runtime. `None` when no scripts directory is configured or
    /// when initialisation failed — the game operates normally without hooks.
    lua: Option<LuaRuntime>,
    /// XML item-to-script registry parsed from `actions.xml` / `movements.xml`.
    registry: XmlRegistry,
    /// Monster type blueprints keyed by name (e.g. "Ice Golem").
    monster_types: HashMap<String, MonsterType>,
    /// Current monotonic time in milliseconds. Updated by tick commands
    /// (CombatTick, RegenerationTick, etc.). Used by time-sensitive operations
    /// like food regeneration and condition management.
    now_ms: u64,
}

/// Configuration for the game actor passed from [`spawn`].
#[derive(Default)]
pub struct GameConfig {
    /// Directory containing `.lua` scripts for the Lua runtime.
    /// `None` disables scripting at runtime.
    pub lua_scripts_dir: Option<PathBuf>,
    /// XML content of `actions.xml` mapping item ids to script hooks.
    pub actions_xml: String,
    /// XML content of `config/monsters.xml` — monster type blueprints.
    pub monsters_xml: String,
    /// XML content of `world/map-spawn.xml` — monster spawn points.
    pub spawns_xml: String,
    /// Directory containing individual monster XML files (e.g. `data/monster/`).
    /// When set, these files are parsed and merged into `monster_types`,
    /// providing race attributes and overrides for the flat config entries.
    pub monsters_data_dir: Option<PathBuf>,
}

impl Game {
    fn new(chunks: ChunkManager, meta: Arc<WorldMeta>) -> Self {
        Game {
            chunks,
            meta,
            dynamic: std::collections::HashMap::new(),
            players: HashMap::new(),
            monsters: HashMap::new(),
            next_id: 0x1000_0000,
            next_monster_id: 0x4000_0000,
            next_spawn_id: 1,
            spawns: HashMap::new(),
            next_statement_id: 1,
            rng: StdRng::from_entropy(),
            save_tx: None,
            lua: None,
            registry: XmlRegistry::default(),
            monster_types: HashMap::new(),
            now_ms: 0,
        }
    }

    /// Create a `Game` from a `StaticMap` — convenience for tests. Not available
    /// in production code (the production path builds ChunkManager directly).
    #[cfg(test)]
    pub(crate) fn from_static_map(map: crate::map::StaticMap) -> Self {
        let (chunks, meta) = map.into_chunks_and_meta();
        Game::new(chunks, Arc::new(meta))
    }

    /// Create a `Game` from an `Arc<StaticMap>` — convenience for tests using
    /// shared fixtures that return `Arc<StaticMap>`.
    #[cfg(test)]
    pub(crate) fn from_static_map_arc(map: std::sync::Arc<crate::map::StaticMap>) -> Self {
        let map = std::sync::Arc::try_unwrap(map).unwrap_or_else(|arc| (*arc).clone());
        Self::from_static_map(map)
    }

    /// Create a `Game` with a fixed RNG seed — deterministic in tests.
    #[cfg(test)]
    #[allow(dead_code)]
    fn new_seeded(chunks: ChunkManager, meta: Arc<WorldMeta>, seed: u64) -> Self {
        Game {
            chunks,
            meta,
            dynamic: std::collections::HashMap::new(),
            players: HashMap::new(),
            monsters: HashMap::new(),
            next_id: 0x1000_0000,
            next_monster_id: 0x4000_0000,
            next_spawn_id: 1,
            spawns: HashMap::new(),
            next_statement_id: 1,
            rng: StdRng::seed_from_u64(seed),
            save_tx: None,
            lua: None,
            registry: XmlRegistry::default(),
            monster_types: HashMap::new(),
            now_ms: 0,
        }
    }

    /// Parse spawns XML, register spawn points, and create initial monsters.
    fn load_spawns(&mut self, xml: &str, _now_ms: u64) {
        let entries = match parse_spawns_xml(xml) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(%e, "failed to parse spawns.xml — no monster spawns loaded");
                return;
            }
        };
        let mut warned_types = HashSet::new();
        for spawn in entries {
            // Case-insensitive lookup in monster type registry.
            if let Some(t) = self.monster_types.get(&spawn.name.to_ascii_lowercase()) {
                let sid = self.next_spawn_id;
                self.next_spawn_id += 1;
                let spawn_entry = MonsterSpawn {
                    position: spawn.position,
                    respawn_interval_ms: spawn.respawn_interval_ms,
                    respawn_at_ms: None,
                    name: t.name.clone(),
                    look_type: t.look_type,
                    health: t.health,
                    max_health: t.max_health,
                    speed: t.speed,
                    attack: t.attack,
                    loot: t.loot.clone(),
                    target_distance: t.target_distance,
                    race: t.race,
                };
                self.spawns.insert(sid, spawn_entry);

                // Create the initial monster immediately.
                let mid = self.next_monster_id;
                self.next_monster_id += 1;
                let monster = MonsterState {
                    name: t.name.clone(),
                    position: spawn.position,
                    direction: Direction::South,
                    health: t.health,
                    max_health: t.max_health,
                    speed: t.speed,
                    look_type: t.look_type,
                    attacking: None,
                    last_attack_ms: 0,
                    attack: t.attack,
                    loot: t.loot.clone(),
                    spawn_id: Some(sid),
                    list_walk_dir: VecDeque::new(),
                    follow_target: None,
                    target_distance: t.target_distance,
                    race: t.race,
                };
                self.monsters.insert(mid, monster);
            } else {
                // No type registered → log warning once per type, spawn with defaults.
                if warned_types.insert(spawn.name.to_ascii_lowercase()) {
                    tracing::warn!(name = %spawn.name, "unknown monster type — spawning with hardcoded defaults");
                }
                let sid = self.next_spawn_id;
                self.next_spawn_id += 1;
                self.spawns.insert(sid, spawn.clone());
                let mid = self.next_monster_id;
                self.next_monster_id += 1;
                let monster = MonsterState {
                    name: spawn.name.clone(),
                    position: spawn.position,
                    direction: Direction::South,
                    health: 50,
                    max_health: 50,
                    speed: 200,
                    look_type: name_hash_looktype(&spawn.name),
                    attacking: None,
                    last_attack_ms: 0,
                    attack: 7,
                    loot: vec![],
                    spawn_id: Some(sid),
                    list_walk_dir: VecDeque::new(),
                    follow_target: None,
                    target_distance: 0,
                    race: Some(RaceType::Blood),
                };
                self.monsters.insert(mid, monster);
            }
        }
        tracing::info!(count = %self.spawns.len(), monsters = %self.monsters.len(), "spawns loaded");
    }

    /// A merged read view (overlay + static) for the map encoder.
    fn merged(&self) -> crate::map::MergedTiles<'_, ChunkManager> {
        crate::map::MergedTiles {
            base: &self.chunks,
            dynamic: &self.dynamic,
        }
    }

    /// Ensure `pos` has a dynamic (owned, mutable) stack, cloning the static one
    /// on first touch. Returns `false` if the tile has no stack at all.
    fn materialize(&mut self, pos: Position) -> bool {
        let key = (pos.x, pos.y, pos.z);
        if self.dynamic.contains_key(&key) {
            return true;
        }
        match self.chunks.tile_stack_clone(pos) {
            Some(st) => {
                self.dynamic.insert(key, st);
                true
            }
            None => false,
        }
    }

    /// Can a viewer at `viewer` see tile `target`? Mirrors TFS
    /// `ProtocolGame::canSee` (`protocolgame.cpp:734-758`) exactly. The client
    /// renders an 18x14 map description anchored at center-8 / center-6, so the
    /// viewport is ASYMMETRIC — one extra column east, one extra row south
    /// (dx in -8..=9, dy in -6..=7). An OVERGROUND viewer (z <= 7) sees every
    /// floor 7→0 (only underground z>7 is hidden); an UNDERGROUND viewer (z >= 8)
    /// sees the `±2` floor band. Either way other floors project diagonally by
    /// `offsetz = viewer.z - target.z` (the same shift the map encoder applies via
    /// `center_z - nz`), so the x/y window slides with the floor delta. (The M5
    /// "strict same-floor overground" rule was a simplification that broke
    /// cross-floor presence on stairs — a viewer on z7 DOES see a creature climb
    /// to z6.)
    fn can_see(viewer: Position, target: Position) -> bool {
        let z_ok = if viewer.z <= 7 {
            target.z <= 7
        } else {
            (i32::from(viewer.z) - i32::from(target.z)).abs() <= 2
        };
        let offsetz = i32::from(viewer.z) - i32::from(target.z);
        let dx = i32::from(target.x) - i32::from(viewer.x) - offsetz;
        let dy = i32::from(target.y) - i32::from(viewer.y) - offsetz;
        z_ok && (-VIEW_LEFT..=VIEW_RIGHT).contains(&dx) && (-VIEW_UP..=VIEW_DOWN).contains(&dy)
    }

    /// Ids of players within (`rx`, `ry`) tiles of `pos` on the same floor,
    /// excluding `exclude`. Symmetric range; used for the yell radius, not the
    /// view (see [`Self::spectators`] for the asymmetric client viewport).
    fn spectators_in_range(&self, pos: Position, exclude: u32, rx: i32, ry: i32) -> Vec<u32> {
        self.players
            .iter()
            .filter(|&(&id, p)| {
                id != exclude
                    && p.position.z == pos.z
                    && (i32::from(p.position.x) - i32::from(pos.x)).abs() <= rx
                    && (i32::from(p.position.y) - i32::from(pos.y)).abs() <= ry
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Ids of players who can see `pos`, excluding `exclude`. The exact dual of
    /// [`Self::can_see`] (a viewer sees `pos` iff `pos` is in that viewer's
    /// asymmetric viewport), so spectator notifications line up tile-for-tile
    /// with what each client actually renders. Use this to notify watchers OF a
    /// tile; use [`Self::visible_from`] for what a watcher AT a tile sees — under
    /// the asymmetric viewport the two directions differ by a tile.
    ///
    /// Ghost GM mode: if the excluded player (the subject at `pos`) is a ghost GM,
    /// non-GM viewers are filtered out — they cannot see the ghost.
    fn spectators(&self, pos: Position, exclude: u32) -> Vec<u32> {
        let ghost = self.players.get(&exclude).map(|p| p.ghost).unwrap_or(false);
        self.players
            .iter()
            .filter(|&(&id, p)| {
                id != exclude && Self::can_see(p.position, pos) && (!ghost || p.gamemaster)
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Ids of players a viewer standing at `viewer` can see, excluding `exclude`
    /// — the forward direction of [`Self::can_see`]. This is what the moving
    /// player renders in its own view, distinct from [`Self::spectators`].
    ///
    /// Ghost GM mode: non-GM viewers see ghost GMs filtered out.
    /// GM viewers (gamemaster = true) always see ghost GMs.
    fn visible_from(&self, viewer: Position, exclude: u32) -> Vec<u32> {
        let viewer_is_gm = self
            .players
            .get(&exclude)
            .map(|p| p.gamemaster)
            .unwrap_or(false);
        self.players
            .iter()
            .filter(|&(&id, p)| {
                id != exclude && Self::can_see(viewer, p.position) && (viewer_is_gm || !p.ghost)
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// Build the creature-thing bytes for `target` as seen by `viewer`, choosing
    /// `0x62` (short) if the viewer already knows the target, else `0x61` (full)
    /// and recording the target in the viewer's known-set. Works for both players
    /// and monsters as the target. The viewer MUST be a player (monsters have no
    /// known-set or session). Returns `None` if either is gone.
    fn introduce(&mut self, viewer: u32, target: u32) -> Option<Vec<u8>> {
        let name = self.creature_name(target)?.to_owned();
        let dir = self.creature_direction(target)?;
        let outfit = self.creature_outfit(target)?;
        let ctype = self.creature_type_byte(target);
        // Only players have a known-set; monsters are never viewers.
        let known = {
            let v = self.players.get_mut(&viewer)?;
            !v.known.insert(target)
        };
        let hp = self.creature_health_percent(target);
        let spd = self.creature_speed(target);
        // Walkthrough byte: 1 for ghost GMs, 0 for everyone else.
        let walkthrough: u8 = if self.players.get(&target).map(|p| p.ghost).unwrap_or(false) {
            1
        } else {
            0
        };
        let view = CreatureView {
            id: target,
            name: name.as_bytes(),
            health_percent: hp,
            direction: dir.to_byte(),
            outfit,
            light_level: 0,
            light_color: 0,
            speed: spd,
            creature_type: ctype,
            walkthrough,
        };
        Some(creature::add_creature(&view, known, 0))
    }

    /// Drop and re-initialise the Lua runtime so script changes on disk take
    /// effect without restarting the server.
    fn do_reload_lua(&mut self) {
        if let Some(rt) = &mut self.lua {
            rt.reload();
        }
    }

    // -----------------------------------------------------------------
    // M12.1 — Creature-agnostic helpers (players + monsters)
    // -----------------------------------------------------------------

    /// Look up any creature's name, checking players first then monsters.
    fn creature_name(&self, id: u32) -> Option<&str> {
        if let Some(p) = self.players.get(&id) {
            return Some(&p.name);
        }
        self.monsters.get(&id).map(|m| m.name.as_str())
    }

    /// Look up any creature's outfit (monsters get a simple look_type outfit).
    fn creature_outfit(&self, id: u32) -> Option<Outfit> {
        if let Some(p) = self.players.get(&id) {
            return Some(p.outfit);
        }
        self.monsters.get(&id).map(|m| Outfit {
            look_type: m.look_type,
            head: 0,
            body: 0,
            legs: 0,
            feet: 0,
            addons: 0,
            mount: 0,
        })
    }

    /// Look up any creature's direction.
    fn creature_direction(&self, id: u32) -> Option<Direction> {
        if let Some(p) = self.players.get(&id) {
            return Some(p.direction);
        }
        self.monsters.get(&id).map(|m| m.direction)
    }

    /// Look up any creature's position.
    fn creature_position(&self, id: u32) -> Option<Position> {
        if let Some(p) = self.players.get(&id) {
            return Some(p.position);
        }
        self.monsters.get(&id).map(|m| m.position)
    }

    /// Health percent for any creature (0..100).
    fn creature_health_percent(&self, id: u32) -> u8 {
        if let Some(p) = self.players.get(&id) {
            if p.max_health == 0 {
                return 0;
            }
            return ((p.health * 100) / p.max_health).min(100) as u8;
        }
        self.monsters
            .get(&id)
            .map(|m| m.health_percent())
            .unwrap_or(0)
    }

    /// Walk speed for any creature.
    fn creature_speed(&self, id: u32) -> u16 {
        if let Some(p) = self.players.get(&id) {
            return p.speed;
        }
        self.monsters.get(&id).map(|m| m.speed).unwrap_or(200)
    }

    /// Wire creature type byte (player=0, monster=1).
    fn creature_type_byte(&self, id: u32) -> u8 {
        if self.players.contains_key(&id) {
            return creature::CREATURETYPE_PLAYER;
        }
        if self.monsters.contains_key(&id) {
            return creature::CREATURETYPE_MONSTER;
        }
        creature::CREATURETYPE_PLAYER
    }

    /// Does any creature (player or monster) exist with this id?
    fn creature_exists(&self, id: u32) -> bool {
        self.players.contains_key(&id) || self.monsters.contains_key(&id)
    }

    /// Ids of monsters whose position is visible from `viewer`.
    fn monsters_visible_from(&self, viewer: Position) -> Vec<u32> {
        self.monsters
            .iter()
            .filter(|(_, m)| Self::can_see(viewer, m.position))
            .map(|(&id, _)| id)
            .collect()
    }

    /// Ids of monsters standing on `pos`.
    #[allow(dead_code)]
    fn monsters_at(&self, pos: Position) -> Vec<u32> {
        self.monsters
            .iter()
            .filter(|(_, m)| m.position == pos)
            .map(|(&id, _)| id)
            .collect()
    }

    /// Best-effort push to a session. On a full/closed channel the player is
    /// reaped (logged out) so the game loop never blocks and memory never grows
    /// unbounded.
    fn push(&mut self, id: u32, payload: Vec<u8>) {
        let dead = match self.players.get(&id) {
            Some(p) => p.push_tx.try_send(payload).is_err(),
            None => return,
        };
        if dead {
            tracing::warn!(id, "session push failed; reaping player");
            self.logout(id);
        }
    }

    fn handle(&mut self, cmd: Command) {
        // Capture now_ms from any tick command so time-sensitive operations
        // (e.g. do_feed) use a consistent time base.
        if let Command::CombatTick { now_ms } = &cmd {
            self.now_ms = *now_ms;
        }
        if let Command::MonsterAiTick { now_ms, .. } = &cmd {
            self.now_ms = *now_ms;
        }
        if let Command::RegenerationTick { now_ms } = &cmd {
            self.now_ms = *now_ms;
        }

        match cmd {
            Command::Login {
                name,
                initial,
                push_tx,
                reply,
            } => {
                let ack = self.login(name, initial, push_tx);
                let _ = reply.send(ack);
            }
            Command::Logout { id } => self.logout(id),
            Command::Move { id, direction } => {
                // Clear auto-walk state on any manual movement.
                if let Some(p) = self.players.get_mut(&id) {
                    p.follow_target = None;
                    p.go_to_position = None;
                    p.list_walk_dir.clear();
                }
                self.do_move(id, direction);
            }
            Command::Turn { id, direction } => self.do_turn(id, direction),
            Command::Say {
                id,
                speak_type,
                text,
            } => self.do_say(id, speak_type, text),
            Command::SetTarget { id, target_id } => self.do_set_target(id, target_id),
            Command::FollowTarget { id, target_id } => self.do_follow_target(id, target_id),
            Command::GoToPosition { id, target } => self.do_go_to_position(id, target),
            Command::GoToSteps { id, steps } => self.do_go_to_steps(id, steps),
            Command::ClearAutoWalk { id } => self.do_clear_auto_walk(id),
            Command::ChangeOutfit { id, outfit } => self.do_change_outfit(id, outfit),
            Command::RequestOutfit { id } => self.do_request_outfit(id),
            Command::CombatTick { now_ms } => self.on_combat_tick(now_ms),
            Command::MonsterAiTick { bucket, now_ms } => self.on_monster_ai_tick(bucket, now_ms),
            Command::RegenerationTick { now_ms } => self.on_regen_tick(now_ms),
            Command::LookAt {
                id,
                x,
                y,
                z,
                stackpos,
            } => self.do_look(id, x, y, z, stackpos),
            Command::LookBattle { id, target_id } => self.do_look_battle(id, target_id),
            Command::MoveThing {
                id,
                from,
                from_stackpos,
                to,
                count,
            } => self.do_move_thing(id, from, from_stackpos, to, count),
            Command::UseItem {
                id,
                pos_x,
                pos_y,
                pos_z,
                stackpos,
                index,
            } => self.do_use_item(id, pos_x, pos_y, pos_z, stackpos, index),
            Command::CloseContainer { id, cid } => self.do_close_container(id, cid),
            Command::UpArrow { id, cid } => self.do_up_arrow(id, cid),
            Command::Gm { id, text } => self.do_gm_command(id, text),
            Command::ReloadLua => self.do_reload_lua(),
            Command::SweepChunks => {
                let mut required: HashSet<ChunkId> = HashSet::new();
                for p in self.players.values() {
                    required.extend(crate::map::chunks_around(p.position));
                }
                for m in self.monsters.values() {
                    required.extend(crate::map::chunks_around(m.position));
                }
                self.chunks.sweep(&required);
            }
            // Intercepted in the actor loop (it must break the loop + ack);
            // never reaches `handle`. Arm kept for match exhaustiveness.
            Command::Shutdown { .. } => {}
        }
    }

    /// Is a creature (other than `exclude`) standing on `pos`?
    /// M12.1: only players block movement; monsters are not yet solid.
    fn tile_occupied(&self, pos: Position, exclude: u32) -> bool {
        self.players
            .iter()
            .any(|(&pid, p)| pid != exclude && p.position == pos)
    }

    /// The wire stackpos a creature with id `exclude` occupies on `pos`, placed
    /// on top: the tile's item base (TFS `getStackposOfCreature` ground+top
    /// items) plus the other creatures already standing there. Co-occupancy
    /// arises on stair/height landings (FLAG_IGNOREBLOCKCREATURE); the newest
    /// arrival renders on top, matching TFS. Capped at 10 like the wire stack.
    fn creature_stackpos_on(&self, pos: Position, exclude: u32) -> u8 {
        let base =
            self.chunks
                .creature_stackpos(i32::from(pos.x), i32::from(pos.y), i32::from(pos.z));
        let others = self
            .players
            .iter()
            .filter(|(id, p)| **id != exclude && p.position == pos)
            .count()
            + self
                .monsters
                .iter()
                .filter(|(id, m)| **id != exclude && m.position == pos)
                .count();
        (usize::from(base) + others).min(10) as u8
    }

    /// The spawn tile, or the nearest walkable & unoccupied tile in expanding
    /// square rings around it (so co-logins don't stack on one tile). Falls back
    /// to the spawn itself if nothing free is found within the search radius.
    fn free_spawn(&self) -> Position {
        let origin = self.meta.spawn();
        if self.chunks.is_walkable(origin) && !self.tile_occupied(origin, u32::MAX) {
            return origin;
        }
        for r in 1..=5i32 {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs() != r && dy.abs() != r {
                        continue; // ring perimeter only
                    }
                    if let Some(p) = origin.offset(dx, dy) {
                        if self.chunks.is_walkable(p) && !self.tile_occupied(p, u32::MAX) {
                            return p;
                        }
                    }
                }
            }
        }
        origin
    }

    /// Like `free_spawn` but anchored at `origin` and excluding `exclude`. Finds
    /// the nearest walkable, unoccupied tile near `origin`, returning `origin` if
    /// free. Used by `login` so a returning player never lands on an occupied tile.
    fn free_spawn_near(&self, origin: Position, exclude: u32) -> Position {
        if self.chunks.is_walkable(origin) && !self.tile_occupied(origin, exclude) {
            return origin;
        }
        for r in 1..=5i32 {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs() != r && dy.abs() != r {
                        continue;
                    }
                    if let Some(p) = origin.offset(dx, dy) {
                        if self.chunks.is_walkable(p) && !self.tile_occupied(p, exclude) {
                            return p;
                        }
                    }
                }
            }
        }
        origin
    }

    /// Ids of creatures (players + monsters) standing on `pos`, in deterministic
    /// id order.
    fn creatures_on(&self, pos: Position) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .players
            .iter()
            .filter(|(_, p)| p.position == pos)
            .map(|(&pid, _)| pid)
            .chain(
                self.monsters
                    .iter()
                    .filter(|(_, m)| m.position == pos)
                    .map(|(&mid, _)| mid),
            )
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Push a `0xB4` status message explaining why a move was rejected.
    fn push_cannot_move(&mut self, id: u32, text: &str) {
        self.push_status_message(id, text.as_bytes());
    }

    /// Server id at overlay/static stack index `idx` on `pos` (overlay wins).
    fn merged_server_id(&self, pos: Position, idx: usize) -> Option<u16> {
        if let Some(st) = self.dynamic.get(&(pos.x, pos.y, pos.z)) {
            return st.server_ids.get(idx).copied();
        }
        self.chunks.tile_item_server_id(pos, idx)
    }

    /// Stack count at overlay/static stack index `idx` on `pos` (overlay wins).
    fn merged_count(&self, pos: Position, idx: usize) -> Option<u8> {
        if let Some(st) = self.dynamic.get(&(pos.x, pos.y, pos.z)) {
            return st.counts.get(idx).copied().flatten();
        }
        self.chunks.tile_item_count(pos, idx)
    }

    /// Items below a creature on `pos`, overlay-aware (overlay wins over static).
    fn merged_pre_creature_len(&self, pos: Position) -> usize {
        self.dynamic
            .get(&(pos.x, pos.y, pos.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or_else(|| self.chunks.tile_pre_creature_len(pos))
    }

    // -----------------------------------------------------------------
    // M7 combat handlers
    // -----------------------------------------------------------------

    /// Push a `0xB4 MESSAGE_STATUS_SMALL` text message to a single player.
    /// Used for PZ rejection and similar status-bar messages.
    fn push_status_message(&mut self, id: u32, text: &[u8]) {
        let mut w = protocol::message::MessageWriter::new();
        w.write_u8(0xB4);
        w.write_u8(MSG_STATUS_SMALL);
        w.write_string(text);
        self.push(id, w.into_bytes());
    }

    /// Push a `0xB4 MESSAGE_INFO_DESCR` look description to a single player.
    fn push_info_descr(&mut self, id: u32, text: &str) {
        let bytes = text.as_bytes();
        let mut w = protocol::message::MessageWriter::new();
        w.write_u8(0xB4);
        w.write_u8(MSG_INFO_DESCR);
        w.write_string(&bytes[..bytes.len().min(255)]);
        self.push(id, w.into_bytes());
    }

    /// Push a `0xB4 MESSAGE_STATUS_CONSOLE_BLUE` line to a single player — blue,
    /// scrollable console text, private to the session. Used for `/help`. Keep the
    /// payload ASCII: the 10.98 client renders text as Latin-1, so multi-byte
    /// UTF-8 (e.g. an em dash) shows as mojibake.
    fn push_console_blue(&mut self, id: u32, text: &str) {
        let bytes = text.as_bytes();
        let mut w = protocol::message::MessageWriter::new();
        w.write_u8(0xB4);
        w.write_u8(MSG_CONSOLE_BLUE);
        w.write_string(&bytes[..bytes.len().min(255)]);
        self.push(id, w.into_bytes());
    }

    // -----------------------------------------------------------------
    // M12.1 Creature A* — Monster AI
    // -----------------------------------------------------------------

    /// Handle `0xA2` — set or clear the player's auto-follow target.
    ///
    /// `target_id == 0` clears follow and walk queue.
    /// Computes an initial A* path to the target if they are not already adjacent.
    pub(super) fn do_follow_target(&mut self, id: u32, target_id: u32) {
        if target_id == 0 {
            if let Some(p) = self.players.get_mut(&id) {
                p.follow_target = None;
                p.list_walk_dir.clear();
            }
            return;
        }
        // Target must exist and must not be self.
        if target_id == id || !self.creature_exists(target_id) {
            return;
        }
        let target_pos = match self.creature_position(target_id) {
            Some(pos) => pos,
            None => return,
        };
        let player_pos = match self.players.get(&id) {
            Some(p) => p.position,
            None => return,
        };
        if player_pos.z != target_pos.z {
            return; // different floor — can't follow
        }
        // Already adjacent?
        let dx = (i32::from(player_pos.x) - i32::from(target_pos.x)).unsigned_abs();
        let dy = (i32::from(player_pos.y) - i32::from(target_pos.y)).unsigned_abs();
        if dx.max(dy) <= 1 {
            return;
        }

        if let Some(p) = self.players.get_mut(&id) {
            p.follow_target = Some(target_id);
        }

        // Compute initial A* path.
        let creature_positions: Vec<Position> = self
            .monsters
            .values()
            .filter(|m| m.position.z == player_pos.z)
            .map(|m| m.position)
            .chain(
                self.players
                    .iter()
                    .filter(|&(pid, p)| *pid != id && p.position.z == player_pos.z)
                    .map(|(_, p)| p.position),
            )
            .collect();

        let params = pathfinding::FindPathParams {
            full_search: false,
            clear_sight: false,
            max_search_dist: 20,
        };
        let tpos = target_pos;
        let condition: pathfinding::FrozenPathingConditionCall = Box::new(move |pos| {
            let dx = (i32::from(pos.x) - i32::from(tpos.x)).unsigned_abs();
            let dy = (i32::from(pos.y) - i32::from(tpos.y)).unsigned_abs();
            dx.max(dy) <= 1
        });

        let path = self.chunks.get_path_matching(
            player_pos,
            target_pos,
            &creature_positions,
            &params,
            condition,
        );

        if !path.is_empty() {
            if let Some(p) = self.players.get_mut(&id) {
                p.list_walk_dir = path;
            }
        }
    }

    /// Handle `0xBE` — cancel auto-walk and clear goto state.
    /// Clears `go_to_position`, `list_walk_dir`, and the existing `follow_target`.
    pub(super) fn do_clear_auto_walk(&mut self, id: u32) {
        if let Some(p) = self.players.get_mut(&id) {
            p.go_to_position = None;
            p.list_walk_dir.clear();
        }
    }

    /// Handle `0x64` GoTo — validate target (same-floor, walkable, in-viewport,
    /// not-PZ, not-already-there), then compute an initial A* path and fill
    /// `list_walk_dir`.
    pub(super) fn do_go_to_position(&mut self, id: u32, target: Position) {
        let (pos, in_pz) = match self.players.get(&id) {
            Some(p) => (p.position, self.chunks.is_protection_zone(p.position)),
            None => return,
        };
        // Check PZ: cannot start auto-walk from or into PZ.
        if in_pz || self.chunks.is_protection_zone(target) {
            self.push_cannot_move(id, "You cannot walk there.");
            return;
        }
        if pos.z != target.z {
            self.push_cannot_move(id, "You cannot walk to a different floor.");
            return;
        }
        if !self.chunks.is_walkable(target) {
            self.push_cannot_move(id, "You cannot walk there.");
            return;
        }
        if !Self::can_see(pos, target) {
            return; // out of view — silently reject (TFS behavior)
        }
        if pos == target {
            return; // already there — no-op
        }

        // Cancel follow-target and set goto.
        if let Some(p) = self.players.get_mut(&id) {
            p.follow_target = None;
            p.go_to_position = Some(target);
        }

        // Collect creature positions on the same floor for A* penalties.
        let creature_positions: Vec<Position> = self
            .monsters
            .values()
            .filter(|m| m.position.z == pos.z)
            .map(|m| m.position)
            .chain(
                self.players
                    .iter()
                    .filter(|&(pid, p)| *pid != id && p.position.z == pos.z)
                    .map(|(_, p)| p.position),
            )
            .collect();

        let params = pathfinding::FindPathParams {
            full_search: false,
            clear_sight: false,
            max_search_dist: 20,
        };
        let tpos = target;
        let condition: pathfinding::FrozenPathingConditionCall = Box::new(move |p| p == tpos);

        let path =
            self.chunks
                .get_path_matching(pos, target, &creature_positions, &params, condition);

        if !path.is_empty() {
            if let Some(p) = self.players.get_mut(&id) {
                p.list_walk_dir = path;
            }
        } else {
            // No path found — clear goto and notify.
            if let Some(p) = self.players.get_mut(&id) {
                p.go_to_position = None;
            }
            self.push_cannot_move(id, "There is no way.");
        }
    }

    /// Handle raw `0x64` auto-walk steps: derive target from the actor's
    /// authoritative `p.position`, validate, and run A*. Avoids `last_pos`
    /// cache drift because the target is derived from the real position,
    /// not from a stale cache in the reader loop.
    pub(super) fn do_go_to_steps(&mut self, id: u32, steps: Vec<protocol::walk::AutoWalkStep>) {
        let start = match self.players.get(&id) {
            Some(p) => (p.position.x, p.position.y, p.position.z),
            None => return,
        };
        let Some(target_coords) = walk::auto_walk_destination(start, &steps) else {
            return; // overflow
        };
        let target = Position::new(target_coords.0, target_coords.1, target_coords.2);

        // Idempotency guard (Fix 2): skip A* if target is unchanged and queue
        // is still active.
        if let Some(p) = self.players.get(&id) {
            if p.go_to_position == Some(target) && !p.list_walk_dir.is_empty() {
                return;
            }
        }

        self.do_go_to_position(id, target);
    }

    /// Monster AI tick — processes one bucket of monsters (`id % 10 == bucket`)
    /// and player auto-walk for players with a follow target.
    ///
    /// Each tick fires every 100ms, cycling through buckets 0..9 so every monster
    /// is processed once per second.
    pub(super) fn on_monster_ai_tick(&mut self, bucket: u8, now_ms: u64) {
        self.now_ms = now_ms;
        // -----------------------------------------------------------------
        // 1. Monster AI: path-refresh + step
        // -----------------------------------------------------------------
        let monster_ids: Vec<u32> = self
            .monsters
            .iter()
            .filter(|&(&id, _)| id % 10 == bucket as u32)
            .map(|(&id, _)| id)
            .collect();

        for id in monster_ids {
            // --- Path refresh ---
            let (follow_target, needs_refresh, terp, target_distance) = {
                let m = match self.monsters.get(&id) {
                    Some(m) => m,
                    None => continue,
                };
                (
                    m.follow_target,
                    m.follow_target.is_some() && m.list_walk_dir.is_empty(),
                    m.position,
                    m.target_distance,
                )
            };

            if needs_refresh {
                let Some(target_id) = follow_target else {
                    continue;
                };
                let Some(target_pos) = self.creature_position(target_id) else {
                    if let Some(m) = self.monsters.get_mut(&id) {
                        m.follow_target = None;
                    }
                    continue;
                };
                if terp.z != target_pos.z {
                    continue;
                }

                // Collect creature positions for pathfinding penalty.
                let mut creature_positions: Vec<Position> = Vec::new();
                for (&mid, m) in &self.monsters {
                    if mid != id && m.position.z == target_pos.z {
                        creature_positions.push(m.position);
                    }
                }
                for p in self.players.values() {
                    if p.position.z == target_pos.z {
                        creature_positions.push(p.position);
                    }
                }

                let params = pathfinding::FindPathParams {
                    full_search: false,
                    clear_sight: false,
                    max_search_dist: 20,
                };
                let tpos = target_pos;
                let td = target_distance.max(1) as u32;
                let condition: pathfinding::FrozenPathingConditionCall = Box::new(move |pos| {
                    let dx = (i32::from(pos.x) - i32::from(tpos.x)).unsigned_abs();
                    let dy = (i32::from(pos.y) - i32::from(tpos.y)).unsigned_abs();
                    dx.max(dy) <= td
                });

                let path = self.chunks.get_path_matching(
                    terp,
                    target_pos,
                    &creature_positions,
                    &params,
                    condition,
                );
                if !path.is_empty() {
                    if let Some(m) = self.monsters.get_mut(&id) {
                        m.list_walk_dir = path;
                    }
                }
            }

            // --- Pop direction and step ---
            let dir = {
                let m = match self.monsters.get_mut(&id) {
                    Some(m) => m,
                    None => continue,
                };
                m.list_walk_dir.pop_front()
            };
            if let Some(direction) = dir {
                self.do_move_monster(id, direction);
            }
        }

        // -----------------------------------------------------------------
        // 2. Player auto-walk (0xA2 follow + 0x64 GoTo)
        // -----------------------------------------------------------------
        let player_ids: Vec<u32> = self
            .players
            .iter()
            .filter(|(_, p)| p.follow_target.is_some() || p.go_to_position.is_some())
            .map(|(&id, _)| id)
            .collect();

        for id in player_ids {
            // Snapshot state.
            let (follow_target, pos, queue_empty, go_to_target) = {
                let p = match self.players.get(&id) {
                    Some(p) => p,
                    None => continue,
                };
                (
                    p.follow_target,
                    p.position,
                    p.list_walk_dir.is_empty(),
                    p.go_to_position,
                )
            };

            if let Some(target_id) = follow_target {
                // -------------------------------------------------------------
                // 2a. Auto-follow (0xA2)
                // -------------------------------------------------------------

                // Target must still exist.
                let Some(target_pos) = self.creature_position(target_id) else {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.follow_target = None;
                    }
                    continue;
                };

                // Different floor — stop following.
                if pos.z != target_pos.z {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.follow_target = None;
                    }
                    continue;
                }

                // Player in PZ — stop following.
                if self.chunks.is_protection_zone(pos) {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.follow_target = None;
                        p.list_walk_dir.clear();
                    }
                    continue;
                }

                // Target no longer in view — stop following.
                if !Self::can_see(pos, target_pos) {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.follow_target = None;
                        p.list_walk_dir.clear();
                    }
                    continue;
                }

                // Already adjacent — we've arrived.
                let dx = (i32::from(pos.x) - i32::from(target_pos.x)).unsigned_abs();
                let dy = (i32::from(pos.y) - i32::from(target_pos.y)).unsigned_abs();
                if dx.max(dy) <= 1 {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.follow_target = None;
                        p.list_walk_dir.clear();
                    }
                    continue;
                }

                // Path refresh on empty queue.
                if queue_empty {
                    let creature_positions: Vec<Position> = self
                        .monsters
                        .values()
                        .filter(|m| m.position.z == pos.z)
                        .map(|m| m.position)
                        .chain(
                            self.players
                                .iter()
                                .filter(|&(pid, p)| *pid != id && p.position.z == pos.z)
                                .map(|(_, p)| p.position),
                        )
                        .collect();

                    let params = pathfinding::FindPathParams {
                        full_search: false,
                        clear_sight: false,
                        max_search_dist: 20,
                    };
                    let tpos = target_pos;
                    let condition: pathfinding::FrozenPathingConditionCall = Box::new(move |p| {
                        let dx = (i32::from(p.x) - i32::from(tpos.x)).unsigned_abs();
                        let dy = (i32::from(p.y) - i32::from(tpos.y)).unsigned_abs();
                        dx.max(dy) <= 1
                    });

                    let path = self.chunks.get_path_matching(
                        pos,
                        target_pos,
                        &creature_positions,
                        &params,
                        condition,
                    );
                    if !path.is_empty() {
                        if let Some(p) = self.players.get_mut(&id) {
                            p.list_walk_dir = path;
                        }
                    }
                }

                // Step-time gate: speed determines the interval between steps.
                if let Some(p) = self.players.get(&id) {
                    let elapsed = self.now_ms - p.last_walk_ms;
                    if elapsed < step_time(p.speed) {
                        continue;
                    }
                }

                // Pop direction and walk (step-time gate is checked above).
                let dir = {
                    let p = match self.players.get_mut(&id) {
                        Some(p) => p,
                        None => continue,
                    };
                    p.list_walk_dir.pop_front()
                };
                if let Some(direction) = dir {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.last_walk_ms = self.now_ms;
                    }
                    self.do_move(id, direction);
                }
            } else if let Some(target) = go_to_target {
                // -------------------------------------------------------------
                // 2b. Click-to-move GoTo (0x64)
                // -------------------------------------------------------------

                // Player in PZ — stop auto-walk.
                if self.chunks.is_protection_zone(pos) {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.go_to_position = None;
                        p.list_walk_dir.clear();
                    }
                    continue;
                }

                // Arrival detection: exact tile match.
                // The character must step onto the target tile, not stop adjacent.
                if pos == target {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.go_to_position = None;
                        p.list_walk_dir.clear();
                    }
                    continue;
                }

                // Repath on empty queue.
                if queue_empty {
                    let creature_positions: Vec<Position> = self
                        .monsters
                        .values()
                        .filter(|m| m.position.z == pos.z)
                        .map(|m| m.position)
                        .chain(
                            self.players
                                .iter()
                                .filter(|&(pid, p)| *pid != id && p.position.z == pos.z)
                                .map(|(_, p)| p.position),
                        )
                        .collect();

                    let params = pathfinding::FindPathParams {
                        full_search: false,
                        clear_sight: false,
                        max_search_dist: 20,
                    };
                    let tpos = target;
                    let condition: pathfinding::FrozenPathingConditionCall =
                        Box::new(move |p| p == tpos);

                    let path = self.chunks.get_path_matching(
                        pos,
                        target,
                        &creature_positions,
                        &params,
                        condition,
                    );
                    if !path.is_empty() {
                        if let Some(p) = self.players.get_mut(&id) {
                            p.list_walk_dir = path;
                        }
                    } else {
                        // Failed repath — increment counter.
                        let failed = self
                            .players
                            .get(&id)
                            .map(|p| p.failed_repaths.unwrap_or(0) + 1)
                            .unwrap_or(1);
                        if failed >= 3 {
                            if let Some(p) = self.players.get_mut(&id) {
                                p.go_to_position = None;
                                p.list_walk_dir.clear();
                                p.failed_repaths = None;
                            }
                            self.push_cannot_move(id, "There is no way.");
                            continue;
                        }
                        if let Some(p) = self.players.get_mut(&id) {
                            p.failed_repaths = Some(failed);
                        }
                        continue; // skip this tick, try again next tick
                    }
                }

                // Step-time gate: speed determines the interval between steps.
                if let Some(p) = self.players.get(&id) {
                    let elapsed = self.now_ms - p.last_walk_ms;
                    if elapsed < step_time(p.speed) {
                        continue;
                    }
                }

                // Pop direction and walk.
                let dir = {
                    let p = match self.players.get_mut(&id) {
                        Some(p) => p,
                        None => continue,
                    };
                    p.list_walk_dir.pop_front()
                };
                if let Some(direction) = dir {
                    if let Some(p) = self.players.get_mut(&id) {
                        p.last_walk_ms = self.now_ms;
                    }
                    // Check if next step would enter PZ.
                    let next_pos = pos.offset(direction.delta().0, direction.delta().1);
                    if let Some(np) = next_pos {
                        if self.chunks.is_protection_zone(np) {
                            if let Some(p) = self.players.get_mut(&id) {
                                p.go_to_position = None;
                                p.list_walk_dir.clear();
                            }
                            self.push_cannot_move(id, "You cannot walk there.");
                            continue;
                        }
                    }
                    self.do_move(id, direction);
                }
            }
        }
    }

    /// Regeneration tick — fires every 1000ms and processes HP/mana
    /// regeneration for all players with active `ConditionRegeneration`.
    ///
    /// For each player, every active condition's `execute_tick` is
    /// called. Expired conditions are removed. If HP or mana changes,
    /// a `0xA0` stats packet is pushed to the player.
    pub(super) fn on_regen_tick(&mut self, now_ms: u64) {
        let player_ids: Vec<u32> = self.players.keys().copied().collect();
        for pid in player_ids {
            if !self.players.contains_key(&pid) {
                continue;
            }
            let stats_opt = {
                let p = self.players.get_mut(&pid).unwrap();
                let mut hp_change = 0i32;

                p.conditions.retain(|c| !c.is_expired(now_ms));
                for c in &mut p.conditions {
                    hp_change += c.execute_tick(now_ms);
                }

                if hp_change > 0 {
                    p.health = (p.health as i32 + hp_change)
                        .min(p.max_health as i32)
                        .max(0) as u32;
                }

                if hp_change != 0 {
                    Some(enter_world::stats(&enter_world::Stats {
                        health: p.health as u16,
                        max_health: p.max_health as u16,
                        free_capacity: 40_000,
                        total_capacity: 40_000,
                        experience: 0,
                        level: 1,
                        level_percent: 0,
                        mana: 0,
                        max_mana: 0,
                        magic_level: 0,
                        soul: 100,
                        stamina_minutes: 2520,
                        base_speed: p.speed,
                    }))
                } else {
                    None
                }
            };
            if let Some(pkt) = stats_opt {
                self.push(pid, pkt);
            }
        }
    }
}

enum Command {
    Login {
        name: String,
        initial: InitialState,
        push_tx: mpsc::Sender<Vec<u8>>,
        reply: oneshot::Sender<LoginAck>,
    },
    Logout {
        id: u32,
    },
    Move {
        id: u32,
        direction: Direction,
    },
    Turn {
        id: u32,
        direction: Direction,
    },
    Say {
        id: u32,
        speak_type: SpeakType,
        text: String,
    },
    /// Client `0xA1`: set (or clear) the attacker's target. `target_id == 0` clears.
    SetTarget {
        id: u32,
        target_id: u32,
    },
    /// Client `0xA2`: set (or clear) the player's auto-follow target. `target_id == 0` clears.
    FollowTarget {
        id: u32,
        target_id: u32,
    },
    /// Client `0x64`: click-to-move — auto-walk to a resolved target position.
    GoToPosition {
        id: u32,
        target: Position,
    },
    /// Client `0x64`: raw auto-walk steps sent to the actor so it derives the
    /// target from its authoritative `p.position` instead of a stale cache.
    GoToSteps {
        id: u32,
        steps: Vec<protocol::walk::AutoWalkStep>,
    },
    /// Client `0xBE` (ESC): cancel auto-walk and clear goto state.
    ClearAutoWalk {
        id: u32,
    },
    /// Client `0xD3`: apply a new outfit and broadcast `0x8E` to spectators.
    ChangeOutfit {
        id: u32,
        outfit: Outfit,
    },
    /// Client `0xD2`: push `0xC8` outfit-window to the requester only.
    RequestOutfit {
        id: u32,
    },
    /// Global combat tick fired by the `tokio::time::interval` task.
    CombatTick {
        now_ms: u64,
    },
    /// Monster AI tick fired every 100ms — processes one bucket (`id % 10 == bucket`).
    MonsterAiTick {
        bucket: u8,
        now_ms: u64,
    },
    /// Regeneration tick fired every 1000ms — processes HP/mana regen
    /// for all players with active `ConditionRegeneration`.
    RegenerationTick {
        now_ms: u64,
    },
    /// Client `0x8C`: look at the thing at `(x,y,z)` stackpos `stackpos`.
    LookAt {
        id: u32,
        x: u16,
        y: u16,
        z: u8,
        stackpos: u8,
    },
    /// Client `0x8D`: look at a creature in the battle list by id.
    LookBattle {
        id: u32,
        target_id: u32,
    },
    /// Client `0x78`: move a thing from one map position to another (M10.1: ground
    /// items only). `count` is the stackable split amount (ignored for non-stackables).
    MoveThing {
        id: u32,
        from: Position,
        from_stackpos: u8,
        to: Position,
        count: u8,
    },
    /// Chat text beginning with `/` from a player. The actor gates on
    /// `PlayerState.gamemaster`, parses the verb, and dispatches to a GM primitive.
    Gm {
        id: u32,
        text: String,
    },
    /// Client `0x82`: use item (open container). `index` is the client-requested cid.
    UseItem {
        id: u32,
        pos_x: u16,
        pos_y: u16,
        pos_z: u8,
        stackpos: u8,
        index: u8,
    },
    /// Client `0x87`: close a container window.
    CloseContainer {
        id: u32,
        cid: u8,
    },
    /// Client `0x88`: navigate to the parent container (up-arrow button).
    UpArrow {
        id: u32,
        cid: u8,
    },
    /// Graceful shutdown: persist every online player, ack, then stop the actor.
    /// Dropping the actor drops `save_tx`, closing the save channel so the DB
    /// drain task can finish. Handled in the actor loop, not in `handle`.
    Shutdown {
        ack: oneshot::Sender<()>,
    },
    /// Drop and re-initialise the Lua runtime so script changes on disk take
    /// effect without restarting the server.
    ReloadLua,
    /// Periodic sweep: evict chunks that are no longer needed.
    SweepChunks,
}

/// Cloneable handle to the running world.
#[derive(Clone)]
pub struct WorldHandle {
    tx: mpsc::Sender<Command>,
    pub meta: Arc<WorldMeta>,
}

impl WorldHandle {
    /// Register a player. The caller supplies the session's outbound channel and
    /// the initial state (restored from save or defaults). Returns the snapshot
    /// + in-range players to render.
    pub async fn login(
        &self,
        name: String,
        initial: InitialState,
        push_tx: mpsc::Sender<Vec<u8>>,
    ) -> Option<LoginAck> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Login {
                name,
                initial,
                push_tx,
                reply,
            })
            .await
            .ok()?;
        rx.await.ok()
    }

    /// Remove a player (called when its session ends). Fire-and-forget.
    pub async fn logout(&self, id: u32) {
        let _ = self.tx.send(Command::Logout { id }).await;
    }

    /// Request a one-tile step. Result is pushed to the session, not returned.
    pub async fn move_player(&self, id: u32, direction: Direction) {
        let _ = self.tx.send(Command::Move { id, direction }).await;
    }

    /// Request a turn in place. Result is pushed to the session, not returned.
    pub async fn turn_player(&self, id: u32, direction: Direction) {
        let _ = self.tx.send(Command::Turn { id, direction }).await;
    }

    /// Broadcast a chat utterance. Fire-and-forget; the world pushes the
    /// resulting `0xAA` packets to whoever can hear it (including the speaker).
    pub async fn say(&self, id: u32, speak_type: SpeakType, text: String) {
        let _ = self
            .tx
            .send(Command::Say {
                id,
                speak_type,
                text,
            })
            .await;
    }

    /// Set or clear the attacker's melee target (`0xA1`). `target_id == 0` clears.
    /// Fire-and-forget; the world applies the PZ check and fight scheduling.
    pub async fn set_target(&self, id: u32, target_id: u32) {
        let _ = self.tx.send(Command::SetTarget { id, target_id }).await;
    }

    /// Set or clear the player's auto-follow target (`0xA2`). `target_id == 0` clears.
    /// Fire-and-forget; the world computes an initial A* path.
    pub async fn follow_target(&self, id: u32, target_id: u32) {
        let _ = self.tx.send(Command::FollowTarget { id, target_id }).await;
    }

    /// Request click-to-move auto-walk (`0x64` GoTo) to a resolved target. Fire-and-forget.
    pub async fn goto_position(&self, id: u32, target: Position) {
        let _ = self.tx.send(Command::GoToPosition { id, target }).await;
    }

    /// Send raw auto-walk steps (`0x64`) so the actor derives the target from
    /// its authoritative position. Avoids `last_pos` cache drift. Fire-and-forget.
    pub async fn goto_steps(&self, id: u32, steps: Vec<protocol::walk::AutoWalkStep>) {
        let _ = self.tx.send(Command::GoToSteps { id, steps }).await;
    }

    /// Cancel auto-walk (`0xBE` ESC). Fire-and-forget.
    pub async fn clear_auto_walk(&self, id: u32) {
        let _ = self.tx.send(Command::ClearAutoWalk { id }).await;
    }

    /// Apply a new outfit (`0xD3`) and broadcast `0x8E` to all spectators.
    /// Fire-and-forget; the world actor owns the state update.
    pub async fn change_outfit(&self, id: u32, outfit: Outfit) {
        let _ = self.tx.send(Command::ChangeOutfit { id, outfit }).await;
    }

    /// Push the `0xC8` outfit-window to the requester only (`0xD2`).
    /// Fire-and-forget; no reply is expected.
    pub async fn request_outfit(&self, id: u32) {
        let _ = self.tx.send(Command::RequestOutfit { id }).await;
    }

    /// Look at a tile thing (`0x8C`). Fire-and-forget; the world pushes `0xB4`.
    pub async fn look(&self, id: u32, x: u16, y: u16, z: u8, stackpos: u8) {
        let _ = self
            .tx
            .send(Command::LookAt {
                id,
                x,
                y,
                z,
                stackpos,
            })
            .await;
    }

    /// Look at a creature in the battle list (`0x8D`). Fire-and-forget.
    pub async fn look_battle(&self, id: u32, target_id: u32) {
        let _ = self.tx.send(Command::LookBattle { id, target_id }).await;
    }

    /// Move a thing on the map (`0x78`). Fire-and-forget; the world validates and
    /// pushes tile-update packets to spectators (M10.1: ground items only).
    pub async fn move_thing(
        &self,
        id: u32,
        from: Position,
        from_stackpos: u8,
        to: Position,
        count: u8,
    ) {
        let _ = self
            .tx
            .send(Command::MoveThing {
                id,
                from,
                from_stackpos,
                to,
                count,
            })
            .await;
    }

    /// Forward a `/`-prefixed chat line to the world as a GM command. The actor
    /// validates that the sender is a gamemaster before doing anything.
    /// Fire-and-forget; feedback is pushed to the sender as a `0xB4` message.
    pub async fn gm_command(&self, id: u32, text: String) {
        let _ = self.tx.send(Command::Gm { id, text }).await;
    }

    /// Use an item (`0x82`). If the item is a container, opens a window.
    pub async fn use_item(
        &self,
        id: u32,
        pos_x: u16,
        pos_y: u16,
        pos_z: u8,
        stackpos: u8,
        index: u8,
    ) {
        let _ = self
            .tx
            .send(Command::UseItem {
                id,
                pos_x,
                pos_y,
                pos_z,
                stackpos,
                index,
            })
            .await;
    }

    /// Close a container window (`0x87`).
    pub async fn close_container(&self, id: u32, cid: u8) {
        let _ = self.tx.send(Command::CloseContainer { id, cid }).await;
    }

    /// Navigate to the parent container (`0x88` up-arrow).
    pub async fn up_arrow(&self, id: u32, cid: u8) {
        let _ = self.tx.send(Command::UpArrow { id, cid }).await;
    }

    /// Persist every online player, then stop the world actor. Resolves once all
    /// save records are queued on the save channel and the actor has begun
    /// shutting down; the caller must then await the save-drain task so the DB
    /// writes flush before the process exits. Used by graceful shutdown.
    pub async fn shutdown_and_save(&self) {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(Command::Shutdown { ack }).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Send a command to reload all Lua scripts from disk.
    pub fn reload_lua(&self) {
        let _ = self.tx.try_send(Command::ReloadLua);
    }
}

/// The outbound channel a session hands the world at login.
pub fn push_channel() -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    mpsc::channel(PUSH_CAPACITY)
}

/// Spawn the world actor task and return a handle plus the save-record receiver.
///
/// The caller (server `main`) must drain the `UnboundedReceiver<SaveRecord>` in
/// a background task, mapping each record to `persistence::PlayerSave` and
/// awaiting `store.save_player`. Also spawns the single global combat-tick task
/// that sends `Command::CombatTick` every `COMBAT_TICK_MS` milliseconds.
pub fn spawn(
    chunks: ChunkManager,
    meta: Arc<WorldMeta>,
    config: GameConfig,
) -> (WorldHandle, mpsc::UnboundedReceiver<SaveRecord>) {
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    let handle = WorldHandle {
        tx: tx.clone(),
        meta: Arc::clone(&meta),
    };

    // Save channel: unbounded so the actor never blocks on logout.
    let (save_tx, save_rx) = mpsc::unbounded_channel::<SaveRecord>();

    // Combat tick: one global interval task sends CombatTick { now_ms } into
    // the actor. `now_ms` is measured from this spawn instant so the actor has
    // a consistent monotonic reference without touching the system clock.
    let tick_tx = tx.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(COMBAT_TICK_MS));
        iv.tick().await; // consume the immediate first tick
        let start = tokio::time::Instant::now();
        loop {
            iv.tick().await;
            let now_ms = start.elapsed().as_millis() as u64;
            if tick_tx.send(Command::CombatTick { now_ms }).await.is_err() {
                break; // actor dropped → server shutting down
            }
        }
    });

    // Monster AI tick: fires every 100ms, cycling through 10 buckets so each
    // monster is processed once per second. i.e. bucket 0, 1, …, 9, 0, …
    let ai_tx = tx.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(MONSTER_AI_TICK_MS));
        iv.tick().await; // consume the immediate first tick
        let start = tokio::time::Instant::now();
        let mut bucket = 0u8;
        loop {
            iv.tick().await;
            let now_ms = start.elapsed().as_millis() as u64;
            if ai_tx
                .send(Command::MonsterAiTick { bucket, now_ms })
                .await
                .is_err()
            {
                break; // actor dropped → server shutting down
            }
            bucket = (bucket + 1) % 10;
        }
    });

    // Regeneration tick: fires every 1s, sending RegenerationTick
    // that processes HP/mana regen for all players with food conditions.
    let regen_tx = tx.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_millis(REGENERATION_TICK_MS));
        iv.tick().await; // consume the immediate first tick
        let start = tokio::time::Instant::now();
        loop {
            iv.tick().await;
            let now_ms = start.elapsed().as_millis() as u64;
            if regen_tx
                .send(Command::RegenerationTick { now_ms })
                .await
                .is_err()
            {
                break; // actor dropped → server shutting down
            }
        }
    });

    // Sweep tick: fires every 5s, sending SweepChunks to evict stale chunks.
    let sweep_tx = tx.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs(5));
        iv.tick().await; // consume the immediate first tick
        loop {
            iv.tick().await;
            if sweep_tx.send(Command::SweepChunks).await.is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let mut game = Game::new(chunks, meta);
        game.save_tx = Some(save_tx);
        // Pin chunks containing spawn and town temple positions so they are never
        // evicted by the sweep. These are always needed for player login.
        {
            let mut pin_ids: Vec<ChunkId> = Vec::new();
            pin_ids.push(crate::map::chunk_id(game.meta.spawn()));
            for town in &game.meta.towns {
                pin_ids.push(crate::map::chunk_id(Position::new(town.x, town.y, town.z)));
            }
            game.chunks.pin(&pin_ids);
        }
        game.lua = config.lua_scripts_dir.map(|d| LuaRuntime::new(&d));
        game.registry = match XmlRegistry::from_actions_xml(&config.actions_xml) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(%e, "failed to parse actions.xml — using empty registry");
                XmlRegistry::default()
            }
        };
        let mut monster_types = match parse_monsters_xml(&config.monsters_xml) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(%e, "failed to parse monsters.xml — using empty type registry");
                HashMap::new()
            }
        };

        // Merge monsters from data dir (individual XMLs with race attributes).
        if let Some(data_dir) = &config.monsters_data_dir {
            match parse_monsters_data_dir(data_dir) {
                Ok(dir_types) => {
                    // Individual files override/update flat config entries.
                    monster_types.extend(dir_types);
                }
                Err(e) => {
                    tracing::warn!(%e, "failed to parse data/monster — continuing with flat config");
                }
            }
        }

        // Default race = Blood for any monster that still has None.
        for mt in monster_types.values_mut() {
            if mt.race.is_none() {
                mt.race = Some(RaceType::Blood);
            }
        }

        game.monster_types = monster_types;
        game.load_spawns(&config.spawns_xml, 0);
        while let Some(cmd) = rx.recv().await {
            if let Command::Shutdown { ack } = cmd {
                game.save_all();
                let _ = ack.send(());
                break; // drop `game` → drop save_tx → save channel closes
            }
            game.handle(cmd);
        }
    });
    (handle, save_rx)
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    // Movement tests live in game::movement::tests.
    // Core helper tests below (they test Game methods defined in this file).

    #[test]
    fn spectators_within_client_viewport() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (a, _ra) = add_player(&mut g, Position::new(100, 100, 7));
        let (b, _rb) = add_player(&mut g, Position::new(108, 106, 7)); // edge: +8x +6y
        let (c, _rc) = add_player(&mut g, Position::new(109, 100, 7)); // 9 west of its own view: out
        // Overground viewer one floor up: TFS lets it see floor 7 (projected),
        // so it IS a spectator of a z7 tile (this is what makes stair presence work).
        let (d, _rd) = add_player(&mut g, Position::new(100, 100, 6));
        let specs = g.spectators(Position::new(100, 100, 7), a);
        assert!(specs.contains(&b), "edge of viewport is visible");
        assert!(!specs.contains(&c), "beyond the viewport is not visible");
        assert!(
            specs.contains(&d),
            "an overground viewer one floor up sees the z7 tile"
        );
        assert!(!specs.contains(&a), "self excluded");
    }

    #[test]
    fn viewport_is_asymmetric_like_tfs() {
        // The 18x14 client map description anchors at center-8 / center-6, so the
        // viewer sees ONE more column east (+9) and one more row south (+7) than
        // west/north. Mirrors TFS ProtocolGame::canSee (x <= myPos.x + (maxX+1)).
        // A symmetric abs()<=8 check is short by one tile on the +x/+y edge, which
        // misaligns "creature became visible" from the slice that actually reveals
        // it — the creature gets marked known but never transmitted (invisible).
        let c = Position::new(100, 100, 7);
        assert!(
            Game::can_see(c, Position::new(109, 100, 7)),
            "+9 east is visible"
        );
        assert!(
            !Game::can_see(c, Position::new(110, 100, 7)),
            "+10 east is not"
        );
        assert!(
            Game::can_see(c, Position::new(92, 100, 7)),
            "-8 west is visible"
        );
        assert!(
            !Game::can_see(c, Position::new(91, 100, 7)),
            "-9 west is not"
        );
        assert!(
            Game::can_see(c, Position::new(100, 107, 7)),
            "+7 south is visible"
        );
        assert!(
            !Game::can_see(c, Position::new(100, 108, 7)),
            "+8 south is not"
        );
        assert!(
            Game::can_see(c, Position::new(100, 94, 7)),
            "-6 north is visible"
        );
        assert!(
            !Game::can_see(c, Position::new(100, 93, 7)),
            "-7 north is not"
        );
    }

    #[test]
    fn spectators_are_the_dual_of_can_see() {
        // spectators(pos) must be exactly { P : can_see(P, pos) }. A player 9 tiles
        // WEST sees pos on its +9 east edge and so IS a spectator; a player 9 tiles
        // EAST cannot (that would need a +9 west view) and is NOT.
        let mut g = Game::from_static_map_arc(walk_map());
        let (west9, _rw) = add_player(&mut g, Position::new(91, 100, 7)); // pos.x - 9
        let (east9, _re) = add_player(&mut g, Position::new(109, 100, 7)); // pos.x + 9
        let specs = g.spectators(Position::new(100, 100, 7), u32::MAX);
        assert!(
            specs.contains(&west9),
            "a viewer 9 west sees pos at its east edge"
        );
        assert!(!specs.contains(&east9), "a viewer 9 east cannot see pos");
    }

    #[test]
    fn introduce_uses_full_then_short_form() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (viewer, _rv) = add_player(&mut g, Position::new(100, 100, 7));
        let (target, _rt) = add_player(&mut g, Position::new(101, 100, 7));
        let first = g.introduce(viewer, target).unwrap();
        assert_eq!(
            u16::from_le_bytes([first[0], first[1]]),
            0x0061,
            "first sighting is full form"
        );
        let second = g.introduce(viewer, target).unwrap();
        assert_eq!(
            u16::from_le_bytes([second[0], second[1]]),
            0x0062,
            "second is short form"
        );
    }

    #[test]
    fn underground_spectator_sees_within_two_floors() {
        // viewer underground at z=9; targets at z=6 (out, >2) and z=11 (in, =2).
        assert!(
            !Game::can_see(Position::new(100, 100, 9), Position::new(100, 100, 6)),
            "3 floors below: out"
        );
        assert!(
            Game::can_see(Position::new(100, 100, 9), Position::new(100, 100, 11)),
            "2 floors below: in"
        );
        assert!(
            Game::can_see(Position::new(100, 100, 9), Position::new(100, 100, 7)),
            "2 floors above: in"
        );
    }

    #[test]
    fn monster_creature_name_rat() {
        let mut g = Game::from_static_map_arc(walk_map());
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        assert_eq!(g.creature_name(mid), Some("Rat"));
    }

    #[test]
    fn monster_creature_outfit_uses_look_type() {
        let mut g = Game::from_static_map_arc(walk_map());
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        let outfit = g.creature_outfit(mid).unwrap();
        assert_eq!(outfit.look_type, 100);
        assert_eq!(outfit.head, 0);
        assert_eq!(outfit.body, 0);
    }

    #[test]
    fn monster_creature_type_byte_returns_monster() {
        let mut g = Game::from_static_map_arc(walk_map());
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        assert_eq!(
            g.creature_type_byte(mid),
            protocol::creature::CREATURETYPE_MONSTER
        );
    }

    #[test]
    fn monster_creature_type_byte_player_returns_player() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, _) = add_player(&mut g, Position::new(100, 100, 7));
        assert_eq!(
            g.creature_type_byte(pid),
            protocol::creature::CREATURETYPE_PLAYER
        );
    }

    #[test]
    fn monster_creature_exists_for_monster() {
        let mut g = Game::from_static_map_arc(walk_map());
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        assert!(g.creature_exists(mid));
        assert!(!g.creature_exists(0x9999_9999));
    }

    #[test]
    fn monster_visible_from_returns_monster_in_range() {
        let mut g = Game::from_static_map_arc(walk_map());
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        let visible = g.monsters_visible_from(Position::new(100, 100, 7));
        assert!(visible.contains(&mid), "monster at +1x must be visible");
    }

    #[test]
    fn monster_not_visible_from_out_of_range() {
        let mut g = Game::from_static_map_arc(walk_map());
        add_monster(&mut g, Position::new(200, 200, 7));
        let visible = g.monsters_visible_from(Position::new(100, 100, 7));
        assert!(visible.is_empty(), "monster too far must not be visible");
    }

    #[test]
    fn monster_creatures_on_includes_monster() {
        let mut g = Game::from_static_map_arc(walk_map());
        let pid = {
            let (id, _) = add_player(&mut g, Position::new(100, 100, 7));
            id
        };
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        let creatures = g.creatures_on(Position::new(101, 100, 7));
        assert!(
            creatures.contains(&mid),
            "monster must be listed on its tile"
        );
        assert!(
            !creatures.contains(&pid),
            "different tile creature excluded"
        );
    }

    #[test]
    fn monster_creature_stackpos_advances_for_monster() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, _) = add_player(&mut g, Position::new(101, 100, 7));
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        // Both on same tile; stackpos = base (1 for ground) + OTHER creatures count.
        // Player was added first, monster second. For the player: 1 other (monster) = 2.
        // For the monster: 1 other (player) = 2 as well.
        let p_sp = g.creature_stackpos_on(Position::new(101, 100, 7), pid);
        let m_sp = g.creature_stackpos_on(Position::new(101, 100, 7), mid);
        assert_eq!(p_sp, 2, "player shares tile with monster → stackpos 2");
        assert_eq!(m_sp, 2, "monster shares tile with player → stackpos 2");
    }

    #[test]
    fn monster_introduce_uses_full_form_first_time() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (viewer, _) = add_player(&mut g, Position::new(100, 100, 7));
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        let first = g.introduce(viewer, mid).unwrap();
        assert_eq!(
            u16::from_le_bytes([first[0], first[1]]),
            0x0061,
            "first monster sighting is full form"
        );
    }

    #[test]
    fn monster_introduce_uses_monster_creature_type() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (viewer, _) = add_player(&mut g, Position::new(100, 100, 7));
        let mid = add_monster(&mut g, Position::new(101, 100, 7));
        let bytes = g.introduce(viewer, mid).unwrap();
        // creatureType at known offset in unknown (0x61) form:
        // [op 2][removeId 4][id 4][creatureType 1][name...]
        // Byte 10 = first creatureType byte.
        assert_eq!(
            bytes[10],
            protocol::creature::CREATURETYPE_MONSTER,
            "introduce must emit CREATURETYPE_MONSTER (1) at first creatureType byte"
        );
    }

    #[test]
    fn go_to_position_defaults_to_none_on_login() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, _) = add_player(&mut g, Position::new(100, 100, 7));
        assert!(g.players.get(&pid).unwrap().go_to_position.is_none());
    }

    #[test]
    fn clear_auto_walk_clears_goto_position_and_queue() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, _) = add_player(&mut g, Position::new(100, 100, 7));
        let p = g.players.get_mut(&pid).unwrap();
        p.go_to_position = Some(Position::new(105, 100, 7));
        p.list_walk_dir.push_back(Direction::East);
        let _ = p;
        // WHEN ClearAutoWalk is handled
        g.handle(Command::ClearAutoWalk { id: pid });
        // THEN both goto and queue are cleared
        let p = g.players.get(&pid).unwrap();
        assert!(p.go_to_position.is_none());
        assert!(p.list_walk_dir.is_empty());
    }

    #[test]
    fn manual_move_clears_goto_position_and_queue() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        {
            let p = g.players.get_mut(&pid).unwrap();
            p.go_to_position = Some(Position::new(99, 117, 7));
            p.list_walk_dir.push_back(Direction::East);
        }
        // WHEN manual move command issued
        g.handle(Command::Move {
            id: pid,
            direction: Direction::East,
        });
        // NOTE: do_move pushes packets to the receiver; we must drain to keep
        // the channel alive so the player isn't reaped as a dead session.
        while rx.try_recv().is_ok() {}
        // THEN goto and queue are cleared after the move
        let p = g.players.get(&pid).expect("player must still exist");
        assert!(
            p.go_to_position.is_none(),
            "go_to_position must be cleared on manual move"
        );
        assert!(
            p.list_walk_dir.is_empty(),
            "walk queue must be cleared on manual move"
        );
    }

    #[test]
    fn do_go_to_position_rejects_different_floor() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        // WHEN goto is called with a target on z=8 (player is on z=7)
        g.do_go_to_position(pid, Position::new(96, 117, 8));
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();
        // THEN goto must not be set
        assert!(
            p.go_to_position.is_none(),
            "cross-floor goto must be rejected"
        );
    }

    #[test]
    fn do_go_to_position_rejects_unwalkable_tile() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        // (94,117) has a block-solid item in walk_map
        g.do_go_to_position(pid, Position::new(94, 117, 7));
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();
        assert!(
            p.go_to_position.is_none(),
            "unwalkable goto must be rejected"
        );
    }

    #[test]
    fn do_go_to_position_rejects_out_of_viewport() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        // Far beyond viewport
        g.do_go_to_position(pid, Position::new(200, 200, 7));
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();
        assert!(
            p.go_to_position.is_none(),
            "out-of-viewport goto must be rejected"
        );
    }

    #[test]
    fn do_go_to_position_already_there_is_noop() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        // Target is exactly where player stands
        g.do_go_to_position(pid, Position::new(95, 117, 7));
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();
        assert!(
            p.go_to_position.is_none(),
            "same-tile goto must be rejected"
        );
    }

    #[test]
    fn do_go_to_position_sets_target_and_fills_queue_for_walkable_dest() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        // Valid walkable destination within viewport, adjacent tile east
        g.do_go_to_position(pid, Position::new(96, 117, 7));
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();
        // THEN go_to_position is set
        assert_eq!(p.go_to_position, Some(Position::new(96, 117, 7)));
        // AND the walk queue has at least one direction
        assert!(
            !p.list_walk_dir.is_empty(),
            "A* path should fill the walk queue"
        );
    }

    // -------------------------------------------------------------------------
    // Task 1.3-1.4: Auto-walk avoids holes (SDD: pathfinding-avoid-holes)
    // -------------------------------------------------------------------------

    #[test]
    fn do_go_to_position_finds_path_around_hole() {
        // GIVEN a player with a hole tile between start and destination
        let mut g = Game::from_static_map_arc(hole_bypass_map());
        let (pid, mut rx) = add_player(&mut g, Position::new(100, 100, 7));

        // WHEN goto is called with a target past the hole
        g.do_go_to_position(pid, Position::new(102, 100, 7));
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();

        // THEN go_to_position is set (target is reachable via bypass)
        assert_eq!(
            p.go_to_position,
            Some(Position::new(102, 100, 7))
        );
        // AND the walk queue is filled (A* found the detour)
        assert!(
            !p.list_walk_dir.is_empty(),
            "A* must find a detour around the hole tile"
        );
        // AND the path does not step through the hole at (101,100)
        let hole_x = 101i32;
        let hole_y = 100i32;
        let mut cur = Position::new(100, 100, 7);
        for &dir in &p.list_walk_dir {
            let (dx, dy) = dir.delta();
            cur = Position::new(
                (i32::from(cur.x) + dx) as u16,
                (i32::from(cur.y) + dy) as u16,
                cur.z,
            );
            assert!(
                i32::from(cur.x) != hole_x || i32::from(cur.y) != hole_y,
                "path must not route through the hole tile at (101, 100)"
            );
        }
    }

    #[test]
    fn do_follow_target_finds_path_around_hole() {
        // GIVEN a player and a follow target on opposite sides of a hole tile
        let mut g = Game::from_static_map_arc(hole_bypass_map());
        // Player at (100,100,7), follow target at (102,100,7)
        // Hole between them at (101,100,7)
        let (pid, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        let (fid, _rfx) = add_player(&mut g, Position::new(102, 100, 7));
        while rx.try_recv().is_ok() {}

        // WHEN do_follow_target is called
        g.do_follow_target(pid, fid);
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();

        // THEN follow_target is set
        assert_eq!(p.follow_target, Some(fid));
        // AND the walk queue is filled (A* found a detour)
        assert!(
            !p.list_walk_dir.is_empty(),
            "A* must find a detour around the hole tile for follow"
        );
        // AND the path does not step through the hole at (101,100)
        let hole_x = 101i32;
        let hole_y = 100i32;
        let mut cur = Position::new(100, 100, 7);
        for &dir in &p.list_walk_dir {
            let (dx, dy) = dir.delta();
            cur = Position::new(
                (i32::from(cur.x) + dx) as u16,
                (i32::from(cur.y) + dy) as u16,
                cur.z,
            );
            assert!(
                i32::from(cur.x) != hole_x || i32::from(cur.y) != hole_y,
                "follow path must not route through the hole tile at (101, 100)"
            );
        }
    }

    // -------------------------------------------------------------------------
    // AI tick auto-walk tests (Task 3.3)
    // -------------------------------------------------------------------------

    #[test]
    fn do_go_to_position_fills_queue_for_distant_target() {
        // combat_map(false) has tiles at (95..97, 117, 7) all walkable.
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.do_go_to_position(pid, Position::new(97, 117, 7));
        while rx.try_recv().is_ok() {}
        let queue_len = g.players.get(&pid).unwrap().list_walk_dir.len();
        assert!(
            queue_len > 0,
            "queue must be filled for distant target; got {queue_len}"
        );
    }

    #[test]
    fn ai_tick_takes_one_step_toward_go_to_target() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        // Target at (97,117) — 2 tiles east, so arrival check won't trigger
        // until after at least one step.
        g.do_go_to_position(pid, Position::new(97, 117, 7));
        while rx.try_recv().is_ok() {}
        assert!(
            !g.players.get(&pid).unwrap().list_walk_dir.is_empty(),
            "queue not empty"
        );
        let before = g.players.get(&pid).unwrap().position;

        // WHEN AI tick fires
        g.on_monster_ai_tick(0, 1000);
        while rx.try_recv().is_ok() {}

        // THEN player moved one step closer
        let after = g.players.get(&pid).unwrap().position;
        assert_ne!(
            before, after,
            "player should take one step toward goto target"
        );
    }

    #[test]
    fn ai_tick_clears_goto_on_arrival() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        // Set goto to (97,117) — two tiles east. After tick 1: at (96,117).
        // Tick 2: step to (97,117). Tick 3: exact arrival.
        g.do_go_to_position(pid, Position::new(97, 117, 7));
        while rx.try_recv().is_ok() {}
        // Tick 1: step to (96,117)
        g.on_monster_ai_tick(0, 1000);
        while rx.try_recv().is_ok() {}
        // Tick 2: step to (97,117) — exact target tile
        g.on_monster_ai_tick(0, 2000);
        while rx.try_recv().is_ok() {}
        // Tick 3: detect arrival at exact position (97,117)
        g.on_monster_ai_tick(0, 3000);
        while rx.try_recv().is_ok() {}

        let p = g.players.get(&pid).unwrap();
        assert!(p.go_to_position.is_none(), "goto cleared on exact arrival");
    }

    #[test]
    fn ai_tick_clears_goto_on_pz_entry() {
        // wide_combat_map_with_pz: (90,117) is PZ, (91..116,117) non-PZ.
        // Start at (91,117), step west into (90,117)PZ.
        let mut g = Game::from_static_map_arc(wide_combat_map_with_pz());
        let (pid, mut rx) = add_player(&mut g, Position::new(91, 117, 7));
        {
            let p = g.players.get_mut(&pid).unwrap();
            p.go_to_position = Some(Position::new(80, 117, 7));
            p.list_walk_dir.push_back(Direction::West);
        }
        // Tick: step west onto PZ tile (90,117)
        g.on_monster_ai_tick(0, 1000);
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();
        // After stepping into PZ, goto must be cleared
        assert!(
            p.go_to_position.is_none(),
            "goto must be cleared when stepping into PZ"
        );
        assert!(
            p.list_walk_dir.is_empty(),
            "walk queue must be cleared when stepping into PZ"
        );
    }

    // -------------------------------------------------------------------------
    // Fix 1 — `last_pos` cache drift: GoToSteps uses authoritative position
    // -------------------------------------------------------------------------

    #[test]
    fn do_go_to_steps_derives_target_from_authoritative_position() {
        // GIVEN a player at (95,117,7) who moves manually to (96,117,7)
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.handle(Command::Move {
            id: pid,
            direction: Direction::East,
        });
        while rx.try_recv().is_ok() {}
        assert_eq!(
            g.players.get(&pid).unwrap().position,
            Position::new(96, 117, 7)
        );

        // WHEN GoToSteps is called with [East] — target should be (97,117,7)
        // based on the authoritative position (96,117,7).
        let steps = vec![protocol::walk::AutoWalkStep::East];
        g.handle(Command::GoToSteps { id: pid, steps });
        while rx.try_recv().is_ok() {}

        // THEN target must be derived from the authoritative position (96,117)
        // not from the initial spawn (95,117)
        let p = g.players.get(&pid).unwrap();
        assert_eq!(
            p.go_to_position,
            Some(Position::new(97, 117, 7)),
            "GoToSteps must derive target from actor's p.position, not a stale cache"
        );
    }

    // -------------------------------------------------------------------------
    // Fix 2 — Redundant A* guard: same target skips A* on second call
    // -------------------------------------------------------------------------

    #[test]
    fn do_go_to_steps_same_target_skips_astar_on_second_call() {
        let mut g = Game::from_static_map_arc(combat_map(false));
        let (pid, mut rx) = add_player(&mut g, Position::new(95, 117, 7));

        // First call with [East, East] steps: should compute A* and fill queue
        let steps = vec![
            protocol::walk::AutoWalkStep::East,
            protocol::walk::AutoWalkStep::East,
        ];
        g.handle(Command::GoToSteps {
            id: pid,
            steps: steps.clone(),
        });
        while rx.try_recv().is_ok() {}
        let p = g.players.get(&pid).unwrap();
        assert_eq!(p.go_to_position, Some(Position::new(97, 117, 7)));
        let first_queue_len = p.list_walk_dir.len();
        assert!(first_queue_len > 0, "first call should fill the walk queue");

        // Simulate one tick consuming one step
        {
            let p = g.players.get_mut(&pid).unwrap();
            p.list_walk_dir.pop_front();
        }
        let reduced_len = g.players.get(&pid).unwrap().list_walk_dir.len();

        // Second call with same steps: guard should skip A*, queue stays reduced
        g.handle(Command::GoToSteps { id: pid, steps });
        while rx.try_recv().is_ok() {}

        let p = g.players.get(&pid).unwrap();
        assert_eq!(
            p.list_walk_dir.len(),
            reduced_len,
            "second call with same target must NOT re-fill the queue (A* guard)"
        );
    }

    // -------------------------------------------------------------------------
    #[test]
    fn overground_viewer_sees_all_upper_floors_but_not_underground() {
        // TFS canSee: an overground viewer (z<=7) sees every floor 7→0 (so a
        // creature on a higher floor IS visible, projected), but NOT underground.
        assert!(
            Game::can_see(Position::new(100, 100, 7), Position::new(100, 100, 7)),
            "same floor"
        );
        assert!(
            Game::can_see(Position::new(100, 100, 7), Position::new(100, 100, 6)),
            "one floor up is visible"
        );
        // A higher floor projects by offsetz; at the same x/y it slides out of the
        // viewport, but offset back by the projection it is visible.
        assert!(
            Game::can_see(Position::new(100, 100, 7), Position::new(102, 102, 5)),
            "two floors up, projection-aligned, visible"
        );
        assert!(
            !Game::can_see(Position::new(100, 100, 7), Position::new(100, 100, 8)),
            "underground hidden from surface"
        );
    }

    // -------------------------------------------------------------------------
    // Chunked map loading tests (PR 3)
    // -------------------------------------------------------------------------

    #[test]
    fn chunks_around_produces_27_ids_for_center_position() {
        let ids = crate::map::chunks_around(Position::new(1000, 1000, 7));
        // 3×3 grid × 3 floors = 27 chunk ids
        assert_eq!(
            ids.len(),
            27,
            "chunks_around must return exactly 27 chunk ids"
        );
    }

    #[test]
    fn sweep_retains_required_chunks() {
        let map = walk_map();
        let (mut chunks, meta) = Arc::try_unwrap(map).unwrap().into_chunks_and_meta();
        let spawn_chunk = crate::map::chunk_id(meta.spawn());
        let far_pos = Position::new(2000, 2000, 7);

        // Sweep with only spawn chunk required
        let required: HashSet<ChunkId> = [spawn_chunk].into_iter().collect();
        chunks.sweep(&required);

        // Spawn chunk must still be present
        assert!(
            chunks.is_walkable(meta.spawn()),
            "spawn chunk must survive sweep when required"
        );
        // Far chunk should be evicted (no tile there)
        assert!(
            !chunks.is_walkable(far_pos),
            "far chunk must be evicted when not required"
        );
    }

    #[test]
    fn pin_prevents_eviction() {
        let map = walk_map();
        let (mut chunks, meta) = Arc::try_unwrap(map).unwrap().into_chunks_and_meta();
        let spawn_chunk = crate::map::chunk_id(meta.spawn());

        // Pin the spawn chunk, sweep with empty required set
        chunks.pin(&[spawn_chunk]);
        chunks.sweep(&HashSet::new());

        // Pinned chunk must survive even with empty required set
        assert!(
            chunks.is_walkable(meta.spawn()),
            "pinned chunk must survive sweep with empty required set"
        );
    }

    #[tokio::test]
    async fn login_with_chunked_map_and_sweep_active() {
        let map = walk_map();
        let (chunks, meta) = Arc::try_unwrap(map).unwrap().into_chunks_and_meta();
        let meta = Arc::new(meta);
        let (world, _save_rx) = super::spawn(chunks, meta, GameConfig::default());

        // Login a player — should succeed with chunked map
        let (tx, _rx) = push_channel();
        let ack = world
            .login("Hero".into(), default_initial(knight()), tx)
            .await
            .unwrap();
        assert!(ack.snapshot.id > 0, "player must receive a valid id");
    }

    #[test]
    fn teleport_computes_destination_chunks() {
        let mut g = Game::from_static_map_arc(walk_map());
        let (player, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        drain(&mut rx);

        // Teleport to a distant position
        let dest = Position::new(1000, 1000, 7);
        g.do_teleport(player, dest);

        assert_eq!(
            g.players.get(&player).unwrap().position,
            dest,
            "player must be teleported to the destination"
        );
    }

    #[test]
    fn do_teleport_same_position_is_noop() {
        let mut g = Game::from_static_map_arc(walk_map());
        let pos = Position::new(95, 117, 7);
        let (player, mut rx) = add_player(&mut g, pos);
        drain(&mut rx);

        g.do_teleport(player, pos);

        // Player still at same position
        assert_eq!(g.players.get(&player).unwrap().position, pos);
    }

    #[test]
    fn sweep_chunks_command_runs_without_crash() {
        let mut g = Game::from_static_map_arc(walk_map());
        // Add a player so there's something to compute required chunks for
        let (player, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        drain(&mut rx);

        // Execute the SweepChunks command
        g.handle(Command::SweepChunks);

        // Player should be unaffected
        assert!(g.players.contains_key(&player));
    }

    #[test]
    fn spawn_chunks_are_pinned_at_boot() {
        let map = walk_map();
        let (mut chunks, meta) = Arc::try_unwrap(map).unwrap().into_chunks_and_meta();
        let spawn_chunk = crate::map::chunk_id(meta.spawn());

        // Simulate what spawn() does: pin spawn/temple chunks
        let mut pin_ids: Vec<ChunkId> = Vec::new();
        pin_ids.push(spawn_chunk);
        for town in &meta.towns {
            pin_ids.push(crate::map::chunk_id(Position::new(town.x, town.y, town.z)));
        }
        chunks.pin(&pin_ids);

        // Sweep with empty required
        chunks.sweep(&HashSet::new());

        // Pinned spawn must survive
        assert!(
            chunks.is_walkable(meta.spawn()),
            "pinned spawn chunk must survive sweep"
        );
    }

    // -------------------------------------------------------------------------
    // Blood-item-on-hit: RaceType → fluid_subtype mapping tests (1.1 RED)
    // -------------------------------------------------------------------------

    #[test]
    fn race_type_blood_maps_to_fluid_5() {
        assert_eq!(RaceType::Blood.fluid_subtype(), Some(5));
    }

    #[test]
    fn race_type_venom_maps_to_fluid_6() {
        assert_eq!(RaceType::Venom.fluid_subtype(), Some(6));
    }

    #[test]
    fn race_type_undead_does_not_splash() {
        assert_eq!(RaceType::Undead.fluid_subtype(), None);
    }

    #[test]
    fn race_type_fire_does_not_splash() {
        assert_eq!(RaceType::Fire.fluid_subtype(), None);
    }

    #[test]
    fn race_type_energy_does_not_splash() {
        assert_eq!(RaceType::Energy.fluid_subtype(), None);
    }

    // -------------------------------------------------------------------------
    // Blood-item-on-hit: XML race attr parsing tests (2.1 RED)
    // -------------------------------------------------------------------------

    #[test]
    fn parse_monsters_xml_race_attr_blood() {
        let xml = r#"<monsters>
            <monster name="Rat" looktype="100" health="50" max_health="50" speed="200" attack="7" race="blood"/>
        </monsters>"#;
        let map = parse_monsters_xml(xml).unwrap();
        let rat = map.get("rat").expect("Rat must be parsed");
        assert_eq!(rat.race, Some(RaceType::Blood));
    }

    #[test]
    fn parse_monsters_xml_race_attr_venom() {
        let xml = r#"<monsters>
            <monster name="Spider" looktype="38" health="30" max_health="30" speed="120" attack="15" race="venom"/>
        </monsters>"#;
        let map = parse_monsters_xml(xml).unwrap();
        let spider = map.get("spider").expect("Spider must be parsed");
        assert_eq!(spider.race, Some(RaceType::Venom));
    }

    #[test]
    fn parse_monsters_xml_race_attr_missing() {
        let xml = r#"<monsters>
            <monster name="Skeleton" looktype="33" health="50" max_health="50" speed="146" attack="7"/>
        </monsters>"#;
        let map = parse_monsters_xml(xml).unwrap();
        let skel = map.get("skeleton").expect("Skeleton must be parsed");
        assert_eq!(skel.race, Some(RaceType::Blood));
    }

    #[test]
    fn parse_monsters_xml_race_attr_undead() {
        let xml = r#"<monsters>
            <monster name="Ghost" looktype="52" health="80" max_health="80" speed="160" attack="10" race="undead"/>
        </monsters>"#;
        let map = parse_monsters_xml(xml).unwrap();
        let ghost = map.get("ghost").expect("Ghost must be parsed");
        assert_eq!(ghost.race, Some(RaceType::Undead));
    }

    #[test]
    fn parse_monsters_data_dir_parses_individual_xml() {
        use std::io::Write;

        let dir = std::env::temp_dir().join(format!("oxidia_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write index monster.xml
        let mut idx = std::fs::File::create(dir.join("monsters.xml")).unwrap();
        idx.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<monsters>
    <monster name="Rat" file="Rats/rat.xml"/>
    <monster name="Dragon" file="Dragons/dragon.xml"/>
</monsters>"#).unwrap();

        // Create subdirs
        std::fs::create_dir_all(dir.join("Rats")).unwrap();
        std::fs::create_dir_all(dir.join("Dragons")).unwrap();

        // Write individual XMLs
        let mut rat = std::fs::File::create(dir.join("Rats/rat.xml")).unwrap();
        rat.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<monster name="Rat" nameDescription="a rat" race="blood" experience="5" speed="134" manacost="200">
    <health now="20" max="20"/>
    <look type="21" corpse="5964"/>
    <attacks>
        <attack name="melee" interval="2000" skill="15" attack="7"/>
    </attacks>
</monster>"#).unwrap();

        let mut dragon = std::fs::File::create(dir.join("Dragons/dragon.xml")).unwrap();
        dragon.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<monster name="Dragon" nameDescription="a dragon" race="fire" experience="1000" speed="180">
    <health now="300" max="300"/>
    <look type="92" corpse="5991"/>
    <attacks>
        <attack name="melee" interval="2000" min="-30" max="-50"/>
    </attacks>
</monster>"#).unwrap();

        let map = parse_monsters_data_dir(&dir).unwrap();

        // Rat: explicit race=blood
        let rat = map.get("rat").expect("Rat must be parsed");
        assert_eq!(rat.race, Some(RaceType::Blood));
        assert_eq!(rat.health, 20);
        assert_eq!(rat.max_health, 20);
        assert_eq!(rat.look_type, 21);
        assert_eq!(rat.speed, 134);
        assert_eq!(rat.attack, 7);

        // Dragon: explicit race=fire, attack from min/max format
        let dragon = map.get("dragon").expect("Dragon must be parsed");
        assert_eq!(dragon.race, Some(RaceType::Fire));
        assert_eq!(dragon.health, 300);
        assert_eq!(dragon.max_health, 300);
        assert_eq!(dragon.look_type, 92);
        assert_eq!(dragon.speed, 180);
        assert_eq!(dragon.attack, 50); // max.abs() = 50

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
