#![forbid(unsafe_code)]
//! The authoritative game loop — M5 unified-push actor.
//!
//! Each session owns an `mpsc<Vec<u8>>` whose `Sender` lives in the actor.
//! The actor is the single builder of all outbound packets, computes spectators,
//! owns the known-creature set, and broadcasts presence events (login appear,
//! walk move/appear/remove, turn, logout remove).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use rand::{SeedableRng, rngs::StdRng};

use protocol::chat::SpeakType;
use protocol::combat_packets;
use protocol::creature::{self, CreatureView, Outfit};
use protocol::map_description::{PlacedCreature, TileSource, WireItem};
use protocol::outfit as outfit_packets;
use protocol::{enter_world, tile_creature, tile_item, walk};

use crate::map::StaticMap;
use crate::{Direction, Position};

mod chat;
mod combat;
mod containers;
mod gm;
mod items;
mod look;
mod movement;
mod session;
#[cfg(test)]
mod test_support;

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

/// TFS `MESSAGE_STATUS_SMALL = 21` (`const.h:190`): white status-bar message.
/// Used for PZ-rejection ("You may not attack…").
const MSG_STATUS_SMALL: u8 = 21;

/// TFS `MESSAGE_INFO_DESCR = 22`: green look-description message (`const.h:191`).
const MSG_INFO_DESCR: u8 = 22;

/// TFS `MESSAGE_STATUS_CONSOLE_BLUE = 4` (`const.h:182`): blue console text.
/// Used for GM command output (`/help`).
const MSG_CONSOLE_BLUE: u8 = 4;

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
    pub health: u16,
    /// Maximum hit points at login (restored or default 150).
    pub max_health: u16,
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
    pub health: u16,
    /// Maximum hit points.
    pub max_health: u16,
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
    pub health: u16,
    pub max_health: u16,
    /// Character sex: 0 = female, 1 = male (TFS outfits.xml `type` convention).
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
    health: u16,
    /// Maximum hit points (TFS default for a new character = 150).
    max_health: u16,
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
    /// Equipment slots 1..=10, indexed 0..=9. `None` = empty slot.
    inventory: [Option<InvItem>; 10],
    /// Open container windows, indexed by cid (0..=15). `None` = window not open.
    open_containers: [Option<OpenContainer>; 16],
}

struct Game {
    map: Arc<StaticMap>,
    /// Copy-on-write overlay of runtime-modified tile stacks (M10.1). Empty until
    /// the first item move; reads fall back to `map` for untouched tiles.
    dynamic: std::collections::HashMap<(u16, u16, u8), crate::map::TileStack>,
    players: HashMap<u32, PlayerState>,
    next_id: u32,
    next_statement_id: u32,
    /// RNG for combat damage rolls. A single actor-owned RNG keeps the loop
    /// lock-free (no shared state) and is seedable in tests for determinism.
    rng: StdRng,
    /// Channel to the background save worker. `None` in unit tests and until
    /// `spawn()` wires it in. Unbounded so `logout` never blocks the actor.
    save_tx: Option<mpsc::UnboundedSender<SaveRecord>>,
}

impl Game {
    fn new(map: Arc<StaticMap>) -> Self {
        Game {
            map,
            dynamic: std::collections::HashMap::new(),
            players: HashMap::new(),
            next_id: 0x1000_0000,
            next_statement_id: 1,
            rng: StdRng::from_entropy(),
            save_tx: None,
        }
    }

    /// Create a `Game` with a fixed RNG seed — deterministic in tests.
    #[cfg(test)]
    #[allow(dead_code)]
    fn new_seeded(map: Arc<StaticMap>, seed: u64) -> Self {
        Game {
            map,
            dynamic: std::collections::HashMap::new(),
            players: HashMap::new(),
            next_id: 0x1000_0000,
            next_statement_id: 1,
            rng: StdRng::seed_from_u64(seed),
            save_tx: None,
        }
    }

    /// A merged read view (overlay + static) for the map encoder.
    fn merged(&self) -> crate::map::MergedTiles<'_> {
        crate::map::MergedTiles { base: self.map.as_ref(), dynamic: &self.dynamic }
    }

    /// Ensure `pos` has a dynamic (owned, mutable) stack, cloning the static one
    /// on first touch. Returns `false` if the tile has no stack at all.
    fn materialize(&mut self, pos: Position) -> bool {
        let key = (pos.x, pos.y, pos.z);
        if self.dynamic.contains_key(&key) {
            return true;
        }
        match self.map.tile_stack_clone(pos) {
            Some(st) => { self.dynamic.insert(key, st); true }
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
    fn spectators(&self, pos: Position, exclude: u32) -> Vec<u32> {
        self.players
            .iter()
            .filter(|&(&id, p)| id != exclude && Self::can_see(p.position, pos))
            .map(|(&id, _)| id)
            .collect()
    }

    /// Ids of players a viewer standing at `viewer` can see, excluding `exclude`
    /// — the forward direction of [`Self::can_see`]. This is what the moving
    /// player renders in its own view, distinct from [`Self::spectators`].
    fn visible_from(&self, viewer: Position, exclude: u32) -> Vec<u32> {
        self.players
            .iter()
            .filter(|&(&id, p)| id != exclude && Self::can_see(viewer, p.position))
            .map(|(&id, _)| id)
            .collect()
    }

    /// Build the creature-thing bytes for `target` as seen by `viewer`, choosing
    /// `0x62` (short) if the viewer already knows the target, else `0x61` (full)
    /// and recording the target in the viewer's known-set. Returns `None` if
    /// either player is gone.
    fn introduce(&mut self, viewer: u32, target: u32) -> Option<Vec<u8>> {
        let (name, dir, outfit) = {
            let t = self.players.get(&target)?;
            (t.name.clone(), t.direction, t.outfit)
        };
        let known = {
            let v = self.players.get_mut(&viewer)?;
            !v.known.insert(target) // insert returns true if newly added
        };
        let view = CreatureView {
            id: target,
            name: name.as_bytes(),
            health_percent: 100,
            direction: dir.to_byte(),
            outfit,
            light_level: 0,
            light_color: 0,
            speed: 220,
        };
        Some(creature::add_creature(&view, known, 0))
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
        match cmd {
            Command::Login { name, initial, push_tx, reply } => {
                let ack = self.login(name, initial, push_tx);
                let _ = reply.send(ack);
            }
            Command::Logout { id } => self.logout(id),
            Command::Move { id, direction } => self.do_move(id, direction),
            Command::Turn { id, direction } => self.do_turn(id, direction),
            Command::Say { id, speak_type, text } => self.do_say(id, speak_type, text),
            Command::SetTarget { id, target_id } => self.do_set_target(id, target_id),
            Command::ChangeOutfit { id, outfit } => self.do_change_outfit(id, outfit),
            Command::RequestOutfit { id } => self.do_request_outfit(id),
            Command::CombatTick { now_ms } => self.on_combat_tick(now_ms),
            Command::LookAt { id, x, y, z, stackpos } => self.do_look(id, x, y, z, stackpos),
            Command::LookBattle { id, target_id } => self.do_look_battle(id, target_id),
            Command::MoveThing { id, from, from_stackpos, to, count } =>
                self.do_move_thing(id, from, from_stackpos, to, count),
            Command::UseItem { id, pos_x, pos_y, pos_z, stackpos, index } =>
                self.do_use_item(id, pos_x, pos_y, pos_z, stackpos, index),
            Command::CloseContainer { id, cid } => self.do_close_container(id, cid),
            Command::UpArrow { id, cid } => self.do_up_arrow(id, cid),
            Command::Gm { id, text } => self.do_gm_command(id, text),
            // Intercepted in the actor loop (it must break the loop + ack);
            // never reaches `handle`. Arm kept for match exhaustiveness.
            Command::Shutdown { .. } => {}
        }
    }

    /// Is a creature (other than `exclude`) standing on `pos`?
    fn tile_occupied(&self, pos: Position, exclude: u32) -> bool {
        self.players.iter().any(|(&pid, p)| pid != exclude && p.position == pos)
    }

    /// The wire stackpos a creature with id `exclude` occupies on `pos`, placed
    /// on top: the tile's item base (TFS `getStackposOfCreature` ground+top
    /// items) plus the other creatures already standing there. Co-occupancy
    /// arises on stair/height landings (FLAG_IGNOREBLOCKCREATURE); the newest
    /// arrival renders on top, matching TFS. Capped at 10 like the wire stack.
    fn creature_stackpos_on(&self, pos: Position, exclude: u32) -> u8 {
        let base = self.map.creature_stackpos(
            i32::from(pos.x), i32::from(pos.y), i32::from(pos.z));
        let others = self
            .players
            .iter()
            .filter(|(id, p)| **id != exclude && p.position == pos)
            .count();
        (usize::from(base) + others).min(10) as u8
    }

    /// The spawn tile, or the nearest walkable & unoccupied tile in expanding
    /// square rings around it (so co-logins don't stack on one tile). Falls back
    /// to the spawn itself if nothing free is found within the search radius.
    fn free_spawn(&self) -> Position {
        let origin = self.map.spawn();
        if self.map.is_walkable(origin) && !self.tile_occupied(origin, u32::MAX) {
            return origin;
        }
        for r in 1..=5i32 {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs() != r && dy.abs() != r {
                        continue; // ring perimeter only
                    }
                    if let Some(p) = origin.offset(dx, dy) {
                        if self.map.is_walkable(p) && !self.tile_occupied(p, u32::MAX) {
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
        if self.map.is_walkable(origin) && !self.tile_occupied(origin, exclude) {
            return origin;
        }
        for r in 1..=5i32 {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs() != r && dy.abs() != r { continue; }
                    if let Some(p) = origin.offset(dx, dy) {
                        if self.map.is_walkable(p) && !self.tile_occupied(p, exclude) {
                            return p;
                        }
                    }
                }
            }
        }
        origin
    }

    /// Ids of creatures standing on `pos`, in deterministic id order. Under the
    /// ≤1-creature-per-tile invariant the vec is length 0 or 1, so the order is
    /// unambiguous. KNOWN LIMITATION: when 2+ creatures co-occupy a tile (only on
    /// stair/height landings via `FLAG_IGNOREBLOCKCREATURE`), id order can differ
    /// from the wire arrival order, so a look at the top creature may resolve to
    /// the other co-occupant. Both render identically (Level 1, no vocation), so
    /// only the displayed name can swap; deferred until it matters.
    fn creatures_on(&self, pos: Position) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .players
            .iter()
            .filter(|(_, p)| p.position == pos)
            .map(|(&pid, _)| pid)
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
        self.map.tile_item_server_id(pos, idx)
    }

    /// Stack count at overlay/static stack index `idx` on `pos` (overlay wins).
    fn merged_count(&self, pos: Position, idx: usize) -> Option<u8> {
        if let Some(st) = self.dynamic.get(&(pos.x, pos.y, pos.z)) {
            return st.counts.get(idx).copied().flatten();
        }
        self.map.tile_item_count(pos, idx)
    }

    /// Items below a creature on `pos`, overlay-aware (overlay wins over static).
    fn merged_pre_creature_len(&self, pos: Position) -> usize {
        self.dynamic
            .get(&(pos.x, pos.y, pos.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or_else(|| self.map.tile_pre_creature_len(pos))
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

}

enum Command {
    Login { name: String, initial: InitialState, push_tx: mpsc::Sender<Vec<u8>>, reply: oneshot::Sender<LoginAck> },
    Logout { id: u32 },
    Move { id: u32, direction: Direction },
    Turn { id: u32, direction: Direction },
    Say { id: u32, speak_type: SpeakType, text: String },
    /// Client `0xA1`: set (or clear) the attacker's target. `target_id == 0` clears.
    SetTarget { id: u32, target_id: u32 },
    /// Client `0xD3`: apply a new outfit and broadcast `0x8E` to spectators.
    ChangeOutfit { id: u32, outfit: Outfit },
    /// Client `0xD2`: push `0xC8` outfit-window to the requester only.
    RequestOutfit { id: u32 },
    /// Global combat tick fired by the `tokio::time::interval` task.
    CombatTick { now_ms: u64 },
    /// Client `0x8C`: look at the thing at `(x,y,z)` stackpos `stackpos`.
    LookAt { id: u32, x: u16, y: u16, z: u8, stackpos: u8 },
    /// Client `0x8D`: look at a creature in the battle list by id.
    LookBattle { id: u32, target_id: u32 },
    /// Client `0x78`: move a thing from one map position to another (M10.1: ground
    /// items only). `count` is the stackable split amount (ignored for non-stackables).
    MoveThing { id: u32, from: Position, from_stackpos: u8, to: Position, count: u8 },
    /// Chat text beginning with `/` from a player. The actor gates on
    /// `PlayerState.gamemaster`, parses the verb, and dispatches to a GM primitive.
    Gm { id: u32, text: String },
    /// Client `0x82`: use item (open container). `index` is the client-requested cid.
    UseItem { id: u32, pos_x: u16, pos_y: u16, pos_z: u8, stackpos: u8, index: u8 },
    /// Client `0x87`: close a container window.
    CloseContainer { id: u32, cid: u8 },
    /// Client `0x88`: navigate to the parent container (up-arrow button).
    UpArrow { id: u32, cid: u8 },
    /// Graceful shutdown: persist every online player, ack, then stop the actor.
    /// Dropping the actor drops `save_tx`, closing the save channel so the DB
    /// drain task can finish. Handled in the actor loop, not in `handle`.
    Shutdown { ack: oneshot::Sender<()> },
}

/// Cloneable handle to the running world.
#[derive(Clone)]
pub struct WorldHandle {
    tx: mpsc::Sender<Command>,
    pub map: Arc<StaticMap>,
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
        self.tx.send(Command::Login { name, initial, push_tx, reply }).await.ok()?;
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
        let _ = self.tx.send(Command::Say { id, speak_type, text }).await;
    }

    /// Set or clear the attacker's melee target (`0xA1`). `target_id == 0` clears.
    /// Fire-and-forget; the world applies the PZ check and fight scheduling.
    pub async fn set_target(&self, id: u32, target_id: u32) {
        let _ = self.tx.send(Command::SetTarget { id, target_id }).await;
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
        let _ = self.tx.send(Command::LookAt { id, x, y, z, stackpos }).await;
    }

    /// Look at a creature in the battle list (`0x8D`). Fire-and-forget.
    pub async fn look_battle(&self, id: u32, target_id: u32) {
        let _ = self.tx.send(Command::LookBattle { id, target_id }).await;
    }

    /// Move a thing on the map (`0x78`). Fire-and-forget; the world validates and
    /// pushes tile-update packets to spectators (M10.1: ground items only).
    pub async fn move_thing(&self, id: u32, from: Position, from_stackpos: u8, to: Position, count: u8) {
        let _ = self.tx.send(Command::MoveThing { id, from, from_stackpos, to, count }).await;
    }

    /// Forward a `/`-prefixed chat line to the world as a GM command. The actor
    /// validates that the sender is a gamemaster before doing anything.
    /// Fire-and-forget; feedback is pushed to the sender as a `0xB4` message.
    pub async fn gm_command(&self, id: u32, text: String) {
        let _ = self.tx.send(Command::Gm { id, text }).await;
    }

    /// Use an item (`0x82`). If the item is a container, opens a window.
    pub async fn use_item(&self, id: u32, pos_x: u16, pos_y: u16, pos_z: u8, stackpos: u8, index: u8) {
        let _ = self.tx.send(Command::UseItem { id, pos_x, pos_y, pos_z, stackpos, index }).await;
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
pub fn spawn(map: Arc<StaticMap>) -> (WorldHandle, mpsc::UnboundedReceiver<SaveRecord>) {
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    let handle = WorldHandle { tx: tx.clone(), map: Arc::clone(&map) };

    // Save channel: unbounded so the actor never blocks on logout.
    let (save_tx, save_rx) = mpsc::unbounded_channel::<SaveRecord>();

    // Combat tick: one global interval task sends CombatTick { now_ms } into
    // the actor. `now_ms` is measured from this spawn instant so the actor has
    // a consistent monotonic reference without touching the system clock.
    let tick_tx = tx.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(
            std::time::Duration::from_millis(COMBAT_TICK_MS));
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

    tokio::spawn(async move {
        let mut game = Game::new(map);
        game.save_tx = Some(save_tx);
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
    use super::*;
    use super::test_support::*;

    // Movement tests live in game::movement::tests.
    // Core helper tests below (they test Game methods defined in this file).

    #[test]
    fn spectators_within_client_viewport() {
        let mut g = Game::new(walk_map());
        let (a, _ra) = add_player(&mut g, Position::new(100, 100, 7));
        let (b, _rb) = add_player(&mut g, Position::new(108, 106, 7)); // edge: +8x +6y
        let (c, _rc) = add_player(&mut g, Position::new(109, 100, 7)); // 9 west of its own view: out
        // Overground viewer one floor up: TFS lets it see floor 7 (projected),
        // so it IS a spectator of a z7 tile (this is what makes stair presence work).
        let (d, _rd) = add_player(&mut g, Position::new(100, 100, 6));
        let specs = g.spectators(Position::new(100, 100, 7), a);
        assert!(specs.contains(&b), "edge of viewport is visible");
        assert!(!specs.contains(&c), "beyond the viewport is not visible");
        assert!(specs.contains(&d), "an overground viewer one floor up sees the z7 tile");
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
        assert!(Game::can_see(c, Position::new(109, 100, 7)), "+9 east is visible");
        assert!(!Game::can_see(c, Position::new(110, 100, 7)), "+10 east is not");
        assert!(Game::can_see(c, Position::new(92, 100, 7)), "-8 west is visible");
        assert!(!Game::can_see(c, Position::new(91, 100, 7)), "-9 west is not");
        assert!(Game::can_see(c, Position::new(100, 107, 7)), "+7 south is visible");
        assert!(!Game::can_see(c, Position::new(100, 108, 7)), "+8 south is not");
        assert!(Game::can_see(c, Position::new(100, 94, 7)), "-6 north is visible");
        assert!(!Game::can_see(c, Position::new(100, 93, 7)), "-7 north is not");
    }

    #[test]
    fn spectators_are_the_dual_of_can_see() {
        // spectators(pos) must be exactly { P : can_see(P, pos) }. A player 9 tiles
        // WEST sees pos on its +9 east edge and so IS a spectator; a player 9 tiles
        // EAST cannot (that would need a +9 west view) and is NOT.
        let mut g = Game::new(walk_map());
        let (west9, _rw) = add_player(&mut g, Position::new(91, 100, 7)); // pos.x - 9
        let (east9, _re) = add_player(&mut g, Position::new(109, 100, 7)); // pos.x + 9
        let specs = g.spectators(Position::new(100, 100, 7), u32::MAX);
        assert!(specs.contains(&west9), "a viewer 9 west sees pos at its east edge");
        assert!(!specs.contains(&east9), "a viewer 9 east cannot see pos");
    }

    #[test]
    fn introduce_uses_full_then_short_form() {
        let mut g = Game::new(walk_map());
        let (viewer, _rv) = add_player(&mut g, Position::new(100, 100, 7));
        let (target, _rt) = add_player(&mut g, Position::new(101, 100, 7));
        let first = g.introduce(viewer, target).unwrap();
        assert_eq!(u16::from_le_bytes([first[0], first[1]]), 0x0061, "first sighting is full form");
        let second = g.introduce(viewer, target).unwrap();
        assert_eq!(u16::from_le_bytes([second[0], second[1]]), 0x0062, "second is short form");
    }

    #[test]
    fn underground_spectator_sees_within_two_floors() {
        // viewer underground at z=9; targets at z=6 (out, >2) and z=11 (in, =2).
        assert!(!Game::can_see(Position::new(100, 100, 9), Position::new(100, 100, 6)), "3 floors below: out");
        assert!(Game::can_see(Position::new(100, 100, 9), Position::new(100, 100, 11)), "2 floors below: in");
        assert!(Game::can_see(Position::new(100, 100, 9), Position::new(100, 100, 7)), "2 floors above: in");
    }

    #[test]
    fn overground_viewer_sees_all_upper_floors_but_not_underground() {
        // TFS canSee: an overground viewer (z<=7) sees every floor 7→0 (so a
        // creature on a higher floor IS visible, projected), but NOT underground.
        assert!(Game::can_see(Position::new(100, 100, 7), Position::new(100, 100, 7)), "same floor");
        assert!(Game::can_see(Position::new(100, 100, 7), Position::new(100, 100, 6)), "one floor up is visible");
        // A higher floor projects by offsetz; at the same x/y it slides out of the
        // viewport, but offset back by the projection it is visible.
        assert!(Game::can_see(Position::new(100, 100, 7), Position::new(102, 102, 5)), "two floors up, projection-aligned, visible");
        assert!(!Game::can_see(Position::new(100, 100, 7), Position::new(100, 100, 8)), "underground hidden from surface");
    }

}
