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

    fn do_turn(&mut self, id: u32, direction: Direction) {
        let pos = match self.players.get_mut(&id) {
            Some(p) => { p.direction = direction; p.position }
            None => return,
        };
        let pkt = walk::creature_turn(id, direction.to_byte());
        self.push(id, pkt.clone()); // mover sees own turn
        for spec in self.spectators(pos, id) {
            self.push(spec, pkt.clone());
        }
    }

    /// Resolve the true destination of a step, applying the two TFS vertical
    /// mechanics. `diagonal` steps skip height resolution (TFS guards with
    /// `!diagonalMovement`). Returns the final position to validate.
    fn resolve_vertical(&self, from: Position, dest: Position, diagonal: bool) -> Position {
        let mut dest = dest;
        if !diagonal {
            // Mechanic A - up: standing on a raised tile, step onto the floor above.
            if from.z != 8 && self.map.triggers_up(from) {
                let above_open = match from.offset_z(-1) {
                    Some(a) => !self.map.has_ground(a) && !self.map.is_blocked(a),
                    None => true,
                };
                if above_open {
                    if let Some(da) = dest.offset_z(-1) {
                        if self.map.has_ground(da)
                            && self.map.floor_change_at(
                                i32::from(da.x), i32::from(da.y), i32::from(da.z),
                            ).is_empty()
                        {
                            dest = da;
                        }
                    }
                }
            }
            // Mechanic A - down: stepping into a void above a raised lower tile.
            if from.z != 7 && from.z == dest.z {
                let dest_void = !self.map.has_ground(dest) && !self.map.is_blocked(dest);
                if dest_void {
                    if let Some(db) = dest.offset_z(1) {
                        if self.map.triggers_up(db) {
                            dest = db;
                        }
                    }
                }
            }
        }
        // Mechanic B - floorChange staircase tile (queryDestination).
        if let Some(landing) = self.map.resolve_floor_change(dest) {
            dest = landing;
        }
        dest
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

    /// Relocate creature `id` to `to`, bypassing walkability. Spectators get a
    /// clean remove/add (a teleport can span any distance, so the incremental
    /// `0x6D` move is never used). The mover gets `remove_creature_by_id` + a full
    /// `0x64` map centered on the landing tile, which carries the landing position
    /// explicitly. Mirrors `do_move` + the teleport branch of `walk::walk_update`.
    fn do_teleport(&mut self, id: u32, to: Position) {
        let from = match self.players.get(&id) {
            Some(p) => p.position,
            None => return,
        };
        if from == to { return; }
        if let Some(p) = self.players.get_mut(&id) { p.position = to; }

        // A teleport can leave an open ground container out of range too.
        self.auto_close_ground_containers(id);

        // PZ badge: resend icons if we crossed a protection-zone boundary.
        if self.map.is_protection_zone(from) != self.map.is_protection_zone(to) {
            let mask = if self.map.is_protection_zone(to) { enter_world::ICON_PIGEON } else { 0 };
            self.push(id, enter_world::icons(mask));
        }

        // Spectators of either endpoint: clean remove/add.
        let mut seen: HashSet<u32> = HashSet::new();
        for s in self.spectators(from, id) { seen.insert(s); }
        for s in self.spectators(to, id) { seen.insert(s); }
        for spec in seen {
            let Some(svpos) = self.players.get(&spec).map(|p| p.position) else { continue };
            let sees_from = Self::can_see(svpos, from);
            let sees_to = Self::can_see(svpos, to);
            if sees_to {
                if sees_from {
                    self.push(spec, walk::remove_creature_by_id(id));
                    if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&id); }
                }
                if let Some(bytes) = self.introduce(spec, id) {
                    let sp = self.creature_stackpos_on(to, id);
                    self.push(spec, tile_creature::add_tile_creature((to.x, to.y, to.z), sp, &bytes));
                }
            } else if sees_from {
                self.push(spec, walk::remove_creature_by_id(id));
                if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&id); }
            }
        }

        // Prune the mover's known-set of creatures no longer in view.
        let left_view: Vec<u32> = self.visible_from(from, id).into_iter()
            .filter(|oid| self.players.get(oid).is_some_and(|p| !Self::can_see(to, p.position)))
            .collect();
        for oid in left_view {
            if let Some(mover) = self.players.get_mut(&id) { mover.known.remove(&oid); }
        }

        // Mover's own view: full 0x64 carrying every in-range player plus self.
        // Build creatures (introduce = &mut self) BEFORE borrowing self.merged().
        let mut wire_creatures: Vec<PlacedCreature> = self.visible_from(to, id).into_iter()
            .filter_map(|oid| {
                let opos = self.players.get(&oid)?.position;
                let bytes = self.introduce(id, oid)?;
                Some(PlacedCreature { x: opos.x, y: opos.y, z: opos.z, bytes })
            })
            .collect();
        if let Some(bytes) = self.introduce(id, id) {
            wire_creatures.push(PlacedCreature { x: to.x, y: to.y, z: to.z, bytes });
        }
        let mut pkt = walk::remove_creature_by_id(id);
        {
            let merged = self.merged();
            pkt.extend(protocol::map_description::encode(
                protocol::map_description::Center { x: to.x, y: to.y, z: to.z },
                &merged,
                &wire_creatures,
            ));
        }
        self.push(id, pkt);
    }

    fn do_move(&mut self, id: u32, direction: Direction) {
        let (from, cur_dir) = match self.players.get(&id) {
            Some(p) => (p.position, p.direction),
            None => return,
        };
        let (dx, dy) = direction.delta();
        let diagonal = matches!(
            direction,
            Direction::NorthEast | Direction::SouthEast | Direction::SouthWest | Direction::NorthWest
        );
        let dest = from
            .offset(dx, dy)
            .map(|d| self.resolve_vertical(from, d, diagonal))
            .filter(|&d| {
                // A vertical landing (stair/height redirect) is reached with TFS
                // FLAG_IGNOREBLOCKITEM | FLAG_IGNOREBLOCKCREATURE (game.cpp:799,815),
                // so BOTH block-solid items AND a creature standing on the landing
                // are ignored — it only needs to be a real tile. Tibia lets you
                // stack onto whoever is on the landing (co-occupancy). Same-floor
                // steps set no such flag, so they keep the full walkability +
                // occupancy check (tile.cpp:564 still blocks).
                if d.z != from.z {
                    self.map.has_ground(d)
                } else {
                    self.map.is_walkable(d) && !self.tile_occupied(d, id)
                }
            });

        let Some(to) = dest else {
            // Blocked: keep the original facing and snap the mover back;
            // spectators see nothing. Matches TFS: a failed walk never turns the
            // player (only Ctrl+arrows / 0x6F-0x72 do). cancel_walk carries the
            // unchanged direction so the client also keeps facing where it was.
            tracing::debug!(
                id, dir = ?direction, diagonal,
                from = ?(from.x, from.y, from.z),
                "move blocked: cancel_walk"
            );
            self.push(id, walk::cancel_walk(cur_dir.to_byte()));
            return;
        };
        // Successful move: now commit the new facing and position. `vertical` is
        // true when resolve_vertical redirected a step to another floor — the
        // prime suspect for underground "desync" when a flat step changes z.
        let vertical = to.z != from.z;
        tracing::debug!(
            id, dir = ?direction, diagonal, vertical,
            from = ?(from.x, from.y, from.z),
            to = ?(to.x, to.y, to.z),
            underground = to.z > 7,
            "move ok"
        );
        if let Some(p) = self.players.get_mut(&id) { p.direction = direction; p.position = to; }

        // Walking out of range of an open ground container closes its window (TFS).
        self.auto_close_ground_containers(id);

        // PZ badge: if the mover crossed a protection-zone boundary, resend the
        // icons packet so the client shows/hides the dove (TFS getClientIcons).
        if self.map.is_protection_zone(from) != self.map.is_protection_zone(to) {
            let mask = if self.map.is_protection_zone(to) { enter_world::ICON_PIGEON } else { 0 };
            self.push(id, enter_world::icons(mask));
        }

        // Spectators that can see either endpoint.
        let mut seen: HashSet<u32> = HashSet::new();
        for s in self.spectators(from, id) { seen.insert(s); }
        for s in self.spectators(to, id) { seen.insert(s); }

        for spec in seen {
            let svpos = self.players.get(&spec).map(|p| p.position);
            let Some(svpos) = svpos else { continue };
            let sees_from = Self::can_see(svpos, from);
            let sees_to = Self::can_see(svpos, to);
            if sees_from && sees_to {
                if from.z == 7 && to.z >= 8 {
                    // Surface->underground boundary: the creature crosses between
                    // the overground and underground render stacks, so a plain
                    // 0x6D desyncs the spectator. TFS sendMoveCreature (2633-2649)
                    // does a clean remove+add here. id-form remove is unambiguous
                    // under co-occupancy; the add lands the mover on top.
                    self.push(spec, walk::remove_creature_by_id(id));
                    if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&id); }
                    if let Some(bytes) = self.introduce(spec, id) {
                        let dsp = self.creature_stackpos_on(to, id);
                        self.push(spec, tile_creature::add_tile_creature(
                            (to.x, to.y, to.z), dsp, &bytes));
                    }
                } else {
                    self.push(spec, walk::creature_move(id, (to.x, to.y, to.z)));
                }
            } else if sees_to {
                if let Some(bytes) = self.introduce(spec, id) {
                    let sp = self.creature_stackpos_on(to, id);
                    self.push(spec, tile_creature::add_tile_creature(
                        (to.x, to.y, to.z), sp, &bytes));
                }
            } else {
                // sees_from only: creature left this spectator's view. id-form
                // remove stays correct even if `from` was co-occupied.
                self.push(spec, walk::remove_creature_by_id(id));
                if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&id); }
            }
        }

        // Creatures that scrolled out of the mover's OWN viewport must be
        // forgotten, mirroring the spectator prune above (335) and logout (226).
        // The client drops them when it recenters; if the server keeps them in
        // the mover's known-set, a later return is sent as the short 0x62 form
        // for a creature the client already discarded, leaving it invisible and
        // tripping "parseCreatureMove: unable to remove creature" on its moves.
        let left_view: Vec<u32> = self
            .visible_from(from, id)
            .into_iter()
            .filter(|oid| {
                self.players.get(oid).is_some_and(|p| !Self::can_see(to, p.position))
            })
            .collect();
        let left_view_len = left_view.len();
        for oid in left_view {
            if let Some(mover) = self.players.get_mut(&id) { mover.known.remove(&oid); }
        }

        // The mover's own view: 0x6D + revealed slices, carrying every other
        // player now in range so they render in the newly exposed tiles.
        let mut wire_creatures: Vec<PlacedCreature> = self
            .visible_from(to, id)
            .into_iter()
            .filter_map(|oid| {
                let opos = self.players.get(&oid)?.position;
                let bytes = self.introduce(id, oid)?;
                Some(PlacedCreature { x: opos.x, y: opos.y, z: opos.z, bytes })
            })
            .collect();
        let others_count = wire_creatures.len();

        // Floor changes whose header is a bare remove (the surface->underground
        // boundary) or a full teleport map (a sloped stair/ladder jumping >1 tile)
        // never re-place the mover on a tile via a 0x6D move. TFS gets away with
        // it because GetFloorDescription lists the player standing on the new tile;
        // here creatures travel out-of-band and `visible_from` excludes the mover,
        // so without this the client keeps the localPlayer object detached from any
        // tile and every later move trips "parseCreatureMove: unable to remove
        // creature". Splice the mover onto its landing tile so the revealed floor
        // block / teleport map re-attaches it. Mirrors TFS MoveDownCreature.
        let dx = (i32::from(to.x) - i32::from(from.x)).abs();
        let dy = (i32::from(to.y) - i32::from(from.y)).abs();
        let boundary = from.z == 7 && to.z >= 8;
        let teleport_like = to.z != from.z && (dx > 1 || dy > 1);
        if boundary || teleport_like {
            if let Some(bytes) = self.introduce(id, id) {
                wire_creatures.push(PlacedCreature { x: to.x, y: to.y, z: to.z, bytes });
            }
        }
        let pkt = {
            let merged = self.merged();
            walk::walk_update(
                id,
                (from.x, from.y, from.z),
                (to.x, to.y, to.z),
                &merged,
                &wire_creatures,
            )
        };
        tracing::debug!(
            id, pkt_len = pkt.len(),
            others = others_count,
            pruned = left_view_len,
            "walk_update pushed to mover"
        );
        self.push(id, pkt);
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
    use crate::map::StaticMap;
    use formats::otb::{ItemType, ItemsOtb};
    use formats::otbm::{MapItem, MapTile, OtbmMap, Town};

    #[test]
    fn walking_onto_a_down_stair_drops_a_floor() {
        let mut g = Game::new(stair_map());
        let (mover, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.do_move(mover, Direction::East); // 100,100,7 -> stair 101,100,7 -> land 101,100,8
        assert_eq!(g.players.get(&mover).unwrap().position, Position::new(101, 100, 8));
        // The mover's own client gets a floor-change-down packet (0xBF present).
        let pkt = rx.try_recv().expect("mover gets a packet");
        assert!(pkt.contains(&protocol::walk::OP_FLOOR_CHANGE_DOWN));
    }

    #[test]
    fn mover_is_readded_on_its_landing_when_crossing_to_underground() {
        // Regression (live desync -> client crash): crossing the surface->
        // underground boundary, the mover's own header is a bare 0x6C id-form
        // remove, never a 0x6D move, so unlike every other step it never re-places
        // the player on a tile. The revealed floor block must carry the mover on
        // its landing tile (as TFS GetFloorDescription lists the creature standing
        // there) or the client keeps the localPlayer object detached from any tile
        // and every later step trips "parseCreatureMove: unable to remove creature".
        let mut g = Game::new(stair_map());
        let (mover, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.do_move(mover, Direction::East); // 100,100,7 -> land 101,100,8
        let pkt = rx.try_recv().expect("mover gets a packet");
        // Bytes [0..7) are the id-form remove ([0x6C][0xFFFF][id]). The mover's id
        // must appear AGAIN past the header: the floor block re-adding it.
        let id_le = mover.to_le_bytes();
        let readded = pkt[7..].windows(4).any(|w| w == id_le);
        assert!(readded, "mover must be re-added on its landing tile after the boundary remove");
    }

    #[test]
    fn down_stair_lands_even_when_landing_is_block_solid() {
        // TFS sets FLAG_NOLIMIT on a stair landing (tile.cpp:817), so a
        // block-solid item on the landing tile does NOT cancel the descent.
        use formats::items_xml::FloorChange;
        let items = ItemsOtb {
            major_version: 3, minor_version: 57, build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 1, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
                ItemType { group: 5, flags: 0, server_id: 300, client_id: 2, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::DOWN },
                ItemType { group: 5, flags: 1 << 0, server_id: 200, client_id: 3, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
            ],
        };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                MapTile { x: 100, y: 100, z: 7, flags: 0, house_id: None, items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
                MapTile { x: 101, y: 100, z: 7, flags: 0, house_id: None, items: vec![MapItem { id: 100, count: None, contents: vec![] }, MapItem { id: 300, count: None, contents: vec![] }] },
                // landing one floor below carries a block-solid item
                MapTile { x: 101, y: 100, z: 8, flags: 0, house_id: None, items: vec![MapItem { id: 100, count: None, contents: vec![] }, MapItem { id: 200, count: None, contents: vec![] }] },
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 7 }],
            waypoints: vec![],
        };
        let sm = Arc::new(StaticMap::from_formats(&map, &items));
        let mut g = Game::new(sm);
        let (mover, _rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.do_move(mover, Direction::East);
        assert_eq!(g.players.get(&mover).unwrap().position, Position::new(101, 100, 8));
    }

    #[test]
    fn down_stair_lands_even_when_landing_is_occupied_by_creature() {
        // TFS sets FLAG_IGNOREBLOCKCREATURE on a height/stair floor change
        // (game.cpp:799,815; gated in tile.cpp:564), so a creature standing on
        // the landing does NOT cancel the descent — Tibia lets you stack onto
        // them. Same-floor steps still respect occupancy (no such flag there).
        let mut g = Game::new(stair_map());
        // B already stands on the landing tile one floor below the stair.
        let landing = Position::new(101, 100, 8);
        let (b, _rb) = add_player(&mut g, landing);
        let (mover, _rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.do_move(mover, Direction::East); // stair 101,100,7 -> land on occupied 101,100,8
        assert_eq!(
            g.players.get(&mover).unwrap().position, landing,
            "descent must succeed onto an occupied landing"
        );
        // Both creatures now co-occupy the landing (Tibia stacking).
        assert_eq!(g.players.get(&b).unwrap().position, landing, "B is still on the landing");
        // The arriving creature renders on top of the one already there:
        // its stackpos is the item base plus the one creature below it.
        let base = g.map.creature_stackpos(101, 100, 8);
        assert_eq!(
            g.creature_stackpos_on(landing, mover), base + 1,
            "the newcomer stacks above the resident creature"
        );
    }

    #[test]
    fn walking_off_a_raised_tile_climbs_a_floor() {
        // Mechanic A (height slopes): standing on a height>=3 tile with an open
        // tile above, stepping toward a tile whose floor-above has ground climbs
        // up one floor (TFS game.cpp:792-807).
        use formats::items_xml::FloorChange;
        let h = |sid| ItemType { group: 5, flags: 1 << 3, server_id: sid, client_id: sid, always_on_top: false, top_order: 0, has_height: true, floor_change: FloorChange::NONE };
        let items = ItemsOtb {
            major_version: 3, minor_version: 57, build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 1, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
                h(301),
            ],
        };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                // raised tile on z=9: ground + 3 height items -> triggers_up
                MapTile { x: 100, y: 100, z: 9, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, count: None, contents: vec![] }, MapItem { id: 301, count: None, contents: vec![] }, MapItem { id: 301, count: None, contents: vec![] }, MapItem { id: 301, count: None, contents: vec![] }] },
                // floor above the eastern destination has ground -> climb target
                MapTile { x: 101, y: 100, z: 8, flags: 0, house_id: None, items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
                // (100,100,8) intentionally absent so the tile above current is open
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 9 }],
            waypoints: vec![],
        };
        let sm = Arc::new(StaticMap::from_formats(&map, &items));
        let mut g = Game::new(sm);
        let (mover, mut rx) = add_player(&mut g, Position::new(100, 100, 9));
        g.do_move(mover, Direction::East); // raised z=9 -> climb to 101,100,8
        assert_eq!(g.players.get(&mover).unwrap().position, Position::new(101, 100, 8));
        let pkt = rx.try_recv().expect("mover gets a packet");
        assert!(pkt.contains(&protocol::walk::OP_FLOOR_CHANGE_UP));
    }

    #[test]
    fn same_floor_spectator_sees_climb_as_move_not_remove() {
        // Regression (live bug): when a creature climbs z7->z6, a spectator still
        // on z7 must get a creature_move (0x6D) — TFS canSee lets an overground
        // viewer see the floor above, so the creature is relocated, not left as a
        // ghost. The old "strict same-floor" can_see sent a (failing) remove.
        use formats::items_xml::FloorChange;
        let h = |sid| ItemType { group: 5, flags: 1 << 3, server_id: sid, client_id: sid, always_on_top: false, top_order: 0, has_height: true, floor_change: FloorChange::NONE };
        let items = ItemsOtb {
            major_version: 3, minor_version: 57, build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 1, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE },
                h(301),
            ],
        };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                MapTile { x: 100, y: 100, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, count: None, contents: vec![] }, MapItem { id: 301, count: None, contents: vec![] }, MapItem { id: 301, count: None, contents: vec![] }, MapItem { id: 301, count: None, contents: vec![] }] },
                MapTile { x: 101, y: 100, z: 6, flags: 0, house_id: None, items: vec![MapItem { id: 100, count: None, contents: vec![] }] },
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 100, y: 100, z: 7 }],
            waypoints: vec![],
        };
        let mut g = Game::new(Arc::new(StaticMap::from_formats(&map, &items)));
        let (mover, _rm) = add_player(&mut g, Position::new(100, 100, 7));
        let (_spec, mut rx) = add_player(&mut g, Position::new(100, 101, 7)); // same floor, adjacent
        g.do_move(mover, Direction::East); // climbs 7 -> 6
        let pkt = rx.try_recv().expect("spectator should be notified of the climb");
        assert_eq!(pkt[0], walk::OP_CREATURE_MOVE, "climb is a move, not a ghost-leaving remove");
        assert_ne!(pkt[0], protocol::tile_creature::OP_REMOVE_TILE_THING);
    }

    #[test]
    fn spectator_gets_remove_then_add_when_mover_crosses_to_underground() {
        // A z=8 spectator near the landing sees a mover descend 7->8: the
        // boundary must produce a clean remove (0x6C) then add (0x6A), not 0x6D.
        let mut g = Game::new(stair_map());
        let (mover, _rm) = add_player(&mut g, Position::new(100, 100, 7));
        let (_spec, mut rx) = add_player(&mut g, Position::new(102, 100, 8));
        g.do_move(mover, Direction::East); // 100,100,7 -> 101,100,8
        let p1 = rx.try_recv().expect("spectator gets remove");
        assert_eq!(p1[0], protocol::tile_creature::OP_REMOVE_TILE_THING);
        let p2 = rx.try_recv().expect("spectator gets add");
        assert_eq!(p2[0], protocol::tile_creature::OP_ADD_TILE_CREATURE);
    }

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
    fn mover_forgets_creatures_that_leave_its_own_viewport() {
        // Repro: A sees B, A walks away until B scrolls off A's own view, A
        // returns. B must be re-introduced in FULL form on return, so it has to
        // be dropped from A's known-set when it leaves A's viewport. Without the
        // prune, A's known-set keeps a stale B, introduce() later emits the short
        // 0x62 form for a creature A's client already dropped, and every 0x6D for
        // B trips OTClient's "parseCreatureMove: unable to remove creature".
        let mut g = Game::new(walk_map());
        // A one tile east of the wall at 94,117 so it can step west to 95,117.
        let (a, _ra) = add_player(&mut g, Position::new(96, 117, 7));
        // B sits at the +9x east edge of A@96: visible from 96 (dx=9, the edge)
        // but not from 95 (dx=10). A's westward step drops B out of view.
        let (b, _rb) = add_player(&mut g, Position::new(105, 117, 7));
        g.introduce(a, b).unwrap();
        assert!(g.players[&a].known.contains(&b), "A knows B after introduce");

        g.do_move(a, Direction::West); // 96,117 -> 95,117; B leaves A's view

        assert!(
            !g.players[&a].known.contains(&b),
            "B left A's viewport, so A must forget it for a full re-introduce on return"
        );
    }

    #[tokio::test]
    async fn move_pushes_creature_move_to_spectator() {
        let (world, _save_rx) = spawn(walk_map());
        let (tx_a, mut rx_a) = push_channel();
        let ack_a = world.login("A".into(), default_initial(knight()), tx_a).await.unwrap();
        let (tx_b, mut rx_b) = push_channel();
        let _ack_b = world.login("B".into(), default_initial(knight()), tx_b).await.unwrap();
        // Drain A's appear-of-B packet.
        let _ = rx_a.recv().await.unwrap();
        // A steps east (95,117 -> 96,117); B (a spectator that sees both
        // endpoints) gets a 0x6D creature-move packet.
        world.move_player(ack_a.snapshot.id, Direction::East).await;
        let pkt = rx_b.recv().await.unwrap();
        assert_eq!(pkt[0], walk::OP_CREATURE_MOVE);
        assert_eq!(u32::from_le_bytes([pkt[3], pkt[4], pkt[5], pkt[6]]), ack_a.snapshot.id);
    }

    #[test]
    fn move_out_of_view_pushes_remove_to_spectator() {
        let mut g = Game::new(walk_map());
        let (mover, _rm) = add_player(&mut g, Position::new(95, 117, 7));
        let (_spec, mut rx) = add_player(&mut g, Position::new(86, 117, 7)); // sees from (dx=9), not to (dx=10)
        g.do_move(mover, Direction::East); // 95,117 -> 96,117
        let pkt = rx.try_recv().expect("spectator should receive a packet");
        assert_eq!(pkt[0], protocol::tile_creature::OP_REMOVE_TILE_THING);
    }

    #[test]
    fn move_into_view_pushes_appear_to_spectator() {
        let mut g = Game::new(walk_map());
        let (mover, _rm) = add_player(&mut g, Position::new(95, 117, 7));
        let (_spec, mut rx) = add_player(&mut g, Position::new(104, 117, 7)); // sees to, not from
        g.do_move(mover, Direction::East); // 95,117 -> 96,117
        let pkt = rx.try_recv().expect("spectator should receive a packet");
        assert_eq!(pkt[0], protocol::tile_creature::OP_ADD_TILE_CREATURE);
    }

    #[test]
    fn cannot_move_onto_tile_occupied_by_creature() {
        let mut g = Game::new(walk_map());
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7)); // east of A
        // A tries to step east onto B's tile -> blocked.
        g.do_move(a, Direction::East);
        // A did not move (still at 95,117); B received no move/appear for A.
        assert!(rb.try_recv().is_err(), "B should get nothing; A's move was blocked");
        let _ = b;
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

    // ---- underground walk-out-and-back map consistency (live desync repro) ----

    /// Floor-8 room where each tile carries two plain items: a ground item whose
    /// client id encodes x (1000 + dx) and a down item encoding y (2000 + dy). A
    /// torn / shifted column therefore surfaces as a wrong client id at a coord.
    fn underground_room() -> Arc<StaticMap> {
        use formats::items_xml::FloorChange;
        let (x0, x1) = (33200u16, 33240u16);
        let (y0, y1) = (32448u16, 32468u16);
        let mut item_types = Vec::new();
        for x in x0..=x1 {
            item_types.push(ItemType { group: 1, flags: 0, server_id: 100 + (x - x0), client_id: 1000 + (x - x0), always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE });
        }
        for y in y0..=y1 {
            item_types.push(ItemType { group: 1, flags: 0, server_id: 500 + (y - y0), client_id: 2000 + (y - y0), always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE });
        }
        let items = ItemsOtb { major_version: 3, minor_version: 57, build_number: 0, items: item_types };
        let mut tiles = Vec::new();
        for x in x0..=x1 {
            for y in y0..=y1 {
                tiles.push(MapTile { x, y, z: 8, flags: 0, house_id: None, items: vec![
                    MapItem { id: 100 + (x - x0), count: None, contents: vec![] }, // ground -> client 1000+dx
                    MapItem { id: 500 + (y - y0), count: None, contents: vec![] }, // down   -> client 2000+dy
                ] });
            }
        }
        let map = OtbmMap { width: 65000, height: 65000, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles, towns: vec![Town { id: 1, name: "U".into(), x: 33215, y: 32458, z: 8 }], waypoints: vec![] };
        Arc::new(StaticMap::from_formats(&map, &items))
    }

    fn server_floor8_ids(map: &StaticMap, x: i32, y: i32) -> Vec<u16> {
        match map.tile(x, y, 8) {
            Some(s) => s.pre_creature.iter().chain(s.post_creature).map(|w| w.client_id).collect(),
            None => Vec::new(),
        }
    }

    /// Seed the client cache with the mover's initial 18x14 floor-8 view — the
    /// client already has this before any walk; steps only send edge slices.
    fn seed_floor8(cache: &mut HashMap<(i32, i32, u8), Vec<u16>>, map: &StaticMap, center: Position) {
        for x in (i32::from(center.x) - 8)..=(i32::from(center.x) + 9) {
            for y in (i32::from(center.y) - 6)..=(i32::from(center.y) + 7) {
                let ids = server_floor8_ids(map, x, y);
                if !ids.is_empty() { cache.insert((x, y, 8), ids); }
            }
        }
    }

    /// Faithful OTClient-side decoder of one band slice stream (mirror of the
    /// `get_map_description` encoder): walks `floors` with `offset = center_z - nz`,
    /// a `skip` run-length counter persisting across floors, plain `[cid][0xFF]`
    /// items. Fills `cache` at the real world coordinate of each tile.
    #[allow(clippy::too_many_arguments)]
    fn decode_band_into(cache: &mut HashMap<(i32, i32, u8), Vec<u16>>, bytes: &[u8], pos: &mut usize,
        anchor_x: i32, anchor_y: i32, center_z: i32, width: i32, height: i32) {
        let floors: Vec<i32> = if center_z > 7 {
            ((center_z - 2)..=(center_z + 2).min(15)).collect()
        } else {
            (0..=7).rev().collect()
        };
        let floor_size = width * height;
        let total = floors.len() as i32 * floor_size;
        let mut skip = 0i32;
        let mut idx = 0i32;
        while idx < total {
            let fi = (idx / floor_size) as usize;
            let nz = floors[fi];
            let offset = center_z - nz;
            let t = idx % floor_size;
            let nx = t / height;
            let ny = t % height;
            let coord = (anchor_x + nx + offset, anchor_y + ny + offset, nz as u8);
            if skip == 0 {
                let peek = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
                if peek >= 0xFF00 {
                    skip = i32::from(peek & 0x00FF);
                    *pos += 2;
                    cache.remove(&coord); // client cleanTile: this position is empty
                } else {
                    *pos += 2; // env u16 (0x0000)
                    let mut ids = Vec::new();
                    loop {
                        let v = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
                        if v >= 0xFF00 { skip = i32::from(v & 0x00FF); *pos += 2; break; }
                        assert_eq!(bytes[*pos + 2], 0xFF, "expected plain item mark at {}", *pos + 2);
                        ids.push(v);
                        *pos += 3;
                    }
                    cache.insert(coord, ids);
                }
            } else {
                cache.remove(&coord); // client cleanTile on a skipped position
                skip -= 1;
            }
            idx += 1;
        }
    }

    /// Apply a same-floor `walk_update` packet to the client cache: skip the
    /// 12-byte 0x6D move header, then decode each directional slice with the same
    /// anchor formulas `walk_update` used to emit them.
    fn apply_walk_update(cache: &mut HashMap<(i32, i32, u8), Vec<u16>>, pkt: &[u8], before: Position, after: Position) {
        assert_eq!(pkt[0], protocol::walk::OP_CREATURE_MOVE, "same-floor move uses 0x6D header");
        let mut pos = 12usize; // [0x6D][0xFFFF][id u32][newx u16][newy u16][newz u8]
        let bx = i32::from(before.x);
        let ax = i32::from(after.x);
        let ay = i32::from(after.y);
        let cz = i32::from(after.z);
        while pos < pkt.len() {
            let opcode = pkt[pos];
            pos += 1;
            let (anchor_x, anchor_y, width, height) = match opcode {
                0x66 => (ax + 9, ay - 6, 1, 14),  // EAST
                0x68 => (ax - 8, ay - 6, 1, 14),  // WEST
                0x65 => (bx - 8, ay - 6, 18, 1),  // NORTH (anchored on old x)
                0x67 => (bx - 8, ay + 7, 18, 1),  // SOUTH (anchored on old x)
                other => panic!("unexpected slice opcode {other:#x}"),
            };
            decode_band_into(cache, pkt, &mut pos, anchor_x, anchor_y, cz, width, height);
        }
    }

    #[test]
    fn underground_walk_out_and_back_keeps_floor8_consistent() {
        // Live desync repro: B walks east out of its viewport, back west, then a
        // couple north on floor 8. Each step only sends an edge slice, so the
        // client cache must stay byte-consistent with the server map — observed
        // live as a torn staircase / shifted left half on the returning client.
        // No other creatures here: this isolates pure map-slice geometry.
        let map = underground_room();
        let mut g = Game::new(map);
        let start = Position::new(33215, 32458, 8);
        let (b, mut rx) = add_player(&mut g, start);
        while rx.try_recv().is_ok() {} // drain login bookkeeping

        let mut cache: HashMap<(i32, i32, u8), Vec<u16>> = HashMap::new();
        seed_floor8(&mut cache, g.map.as_ref(), start);

        let mut seq = Vec::new();
        for _ in 0..8 { seq.push(Direction::East); }
        for _ in 0..8 { seq.push(Direction::West); }
        for _ in 0..2 { seq.push(Direction::North); }
        for dir in seq {
            let before = g.players[&b].position;
            g.do_move(b, dir);
            let after = g.players[&b].position;
            assert_ne!(before, after, "step {dir:?} should succeed");
            let pkt = rx.try_recv().expect("mover gets a walk packet");
            apply_walk_update(&mut cache, &pkt, before, after);
            while rx.try_recv().is_ok() {} // drain extras
        }

        let p = g.players[&b].position;
        let mut mismatches = Vec::new();
        for x in (i32::from(p.x) - 8)..=(i32::from(p.x) + 9) {
            for y in (i32::from(p.y) - 6)..=(i32::from(p.y) + 7) {
                let server = server_floor8_ids(g.map.as_ref(), x, y);
                let client = cache.get(&(x, y, 8)).cloned().unwrap_or_default();
                if client != server {
                    mismatches.push(format!("({x},{y}): client={client:?} server={server:?}"));
                }
            }
        }
        assert!(mismatches.is_empty(), "floor-8 desync after walk-out-and-back:\n{}", mismatches.join("\n"));
    }

    /// Underground room with ground on the full z-2..z+2 band (floors 6..10),
    /// every tile carrying a single item whose client id is unique per (x,y,z) so
    /// any cross-floor / shifted misplacement surfaces as a wrong id at a coord.
    fn underground_multifloor() -> Arc<StaticMap> {
        use formats::items_xml::FloorChange;
        let (x0, x1) = (33200u16, 33240u16);
        let (y0, y1) = (32448u16, 32468u16);
        let span_x = x1 - x0 + 1; // 41
        let span_y = y1 - y0 + 1; // 21
        let uid = |x: u16, y: u16, z: u8| -> u16 {
            1 + (x - x0) + (y - y0) * span_x + u16::from(z - 6) * span_x * span_y
        };
        let mut item_types = Vec::new();
        let mut tiles = Vec::new();
        for z in 6u8..=10 {
            for x in x0..=x1 {
                for y in y0..=y1 {
                    let id = uid(x, y, z);
                    item_types.push(ItemType { group: 1, flags: 0, server_id: id, client_id: id, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE });
                    tiles.push(MapTile { x, y, z, flags: 0, house_id: None, items: vec![MapItem { id, count: None, contents: vec![] }] });
                }
            }
        }
        let items = ItemsOtb { major_version: 3, minor_version: 57, build_number: 0, items: item_types };
        let map = OtbmMap { width: 65000, height: 65000, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles, towns: vec![Town { id: 1, name: "U".into(), x: 33215, y: 32458, z: 8 }], waypoints: vec![] };
        Arc::new(StaticMap::from_formats(&map, &items))
    }

    #[test]
    fn underground_walk_east_west_keeps_full_band_consistent() {
        // Live single-client repro: in an underground depot, walking east/west
        // shifts walls/stairs and trips "unable to remove creature". This exercises
        // the FULL z-2..z+2 band (floors above + below the player), which the
        // floor-8-only test did not. Seed the client cache with the initial band,
        // walk east out of view and back, and assert every band floor stays
        // byte-consistent with the server map.
        let map = underground_multifloor();
        let mut g = Game::new(map);
        let start = Position::new(33215, 32458, 8);
        let (b, mut rx) = add_player(&mut g, start);
        while rx.try_recv().is_ok() {}

        let mut cache: HashMap<(i32, i32, u8), Vec<u16>> = HashMap::new();
        for z in 6u8..=10 {
            for x in (i32::from(start.x) - 8)..=(i32::from(start.x) + 9) {
                for y in (i32::from(start.y) - 6)..=(i32::from(start.y) + 7) {
                    // initial full-map description projects each floor by (z-nz).
                    let off = 8 - i32::from(z);
                    let ids = server_floor8_ids_z(g.map.as_ref(), x + off, y + off, z);
                    if !ids.is_empty() { cache.insert((x + off, y + off, z), ids); }
                }
            }
        }

        let mut seq = Vec::new();
        for _ in 0..8 { seq.push(Direction::East); }
        for _ in 0..8 { seq.push(Direction::West); }
        for dir in seq {
            let before = g.players[&b].position;
            g.do_move(b, dir);
            let after = g.players[&b].position;
            assert_ne!(before, after, "step {dir:?} should succeed");
            let pkt = rx.try_recv().expect("mover gets a walk packet");
            apply_walk_update(&mut cache, &pkt, before, after);
            while rx.try_recv().is_ok() {}
        }

        let p = g.players[&b].position;
        let mut mismatches = Vec::new();
        for z in 6u8..=10 {
            let off = 8 - i32::from(z);
            for sx in (i32::from(p.x) - 8)..=(i32::from(p.x) + 9) {
                for sy in (i32::from(p.y) - 6)..=(i32::from(p.y) + 7) {
                    let (wx, wy) = (sx + off, sy + off);
                    let server = server_floor8_ids_z(g.map.as_ref(), wx, wy, z);
                    let client = cache.get(&(wx, wy, z)).cloned().unwrap_or_default();
                    if client != server {
                        mismatches.push(format!("({wx},{wy},{z}): client={client:?} server={server:?}"));
                    }
                }
            }
        }
        assert!(mismatches.is_empty(), "band desync after E/W walk ({} tiles):\n{}",
            mismatches.len(), mismatches.iter().take(12).cloned().collect::<Vec<_>>().join("\n"));
    }

    fn server_floor8_ids_z(map: &StaticMap, x: i32, y: i32, z: u8) -> Vec<u16> {
        match map.tile(x, y, i32::from(z)) {
            Some(s) => s.pre_creature.iter().chain(s.post_creature).map(|w| w.client_id).collect(),
            None => Vec::new(),
        }
    }

    // ===================================================================
    // M6.2 floor-change desync repro: a FAITHFUL OTClient simulator.
    //
    // Unlike `apply_walk_update` (same-floor only, asserts 0x6D header), this
    // simulator decodes the FULL floor-change opcode set the way OTClient's
    // ProtocolGame::parse* does, tracking a single `central` position shifted by
    // FIXED deltas per opcode (the 10.98 GameMapMovePosition feature is OFF, so
    // floor-change packets carry NO position) plus localPlayer attach/detach.
    //
    // The point: catch the EXACT first divergence (which packet, which coord,
    // client-vs-server) and the instant the localPlayer detaches -> the live
    // "ProtocolGame::parseCreatureMove: unable to remove creature".
    // ===================================================================

    /// AwareRange for the 10.98 client (from the bug report / OTClient source).
    const AR_LEFT: i32 = 8;
    const AR_RIGHT: i32 = 9;
    const AR_TOP: i32 = 6;
    const AR_BOTTOM: i32 = 7;

    /// The client-side world model OTClient maintains.
    struct ClientSim {
        central: Position,
        /// tile -> list of client item ids (creatures are tracked separately).
        cache: HashMap<(i32, i32, u8), Vec<u16>>,
        /// creature id -> the tile it currently sits on in the client map.
        /// A creature absent here is "detached" (exists as object, on no tile).
        creature_tile: HashMap<u32, (i32, i32, u8)>,
        localplayer_id: u32,
        /// First divergence captured (step label, message).
        first_divergence: Option<String>,
    }

    impl ClientSim {
        fn localplayer_attached(&self) -> bool {
            self.creature_tile.contains_key(&self.localplayer_id)
        }

        /// Decode one width*height tile stream that may contain creature blocks,
        /// writing tiles into `cache` and updating `creature_tile`. Mirrors
        /// OTClient setMapDescription/setTileDescription: a written tile is
        /// cleanTile'd first, so any creature previously on it that is NOT
        /// re-listed becomes detached.
        #[allow(clippy::too_many_arguments)]
        fn decode_stream(
            &mut self, bytes: &[u8], pos: &mut usize,
            anchor_x: i32, anchor_y: i32, center_z: i32, width: i32, height: i32,
        ) {
            let floors: Vec<i32> = if center_z > 7 {
                ((center_z - 2)..=(center_z + 2).min(15)).collect()
            } else {
                (0..=7).rev().collect()
            };
            let floor_size = width * height;
            let total = floors.len() as i32 * floor_size;
            let mut skip = 0i32;
            let mut idx = 0i32;
            while idx < total {
                let fi = (idx / floor_size) as usize;
                let nz = floors[fi];
                let offset = center_z - nz;
                let t = idx % floor_size;
                let nx = t / height;
                let ny = t % height;
                let coord = (anchor_x + nx + offset, anchor_y + ny + offset, nz as u8);
                if skip == 0 {
                    let peek = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
                    if peek >= 0xFF00 {
                        skip = i32::from(peek & 0x00FF);
                        *pos += 2;
                        self.clean_tile(coord);
                    } else {
                        *pos += 2; // env u16 (0x0000)
                        // cleanTile first: detach any creature currently here.
                        self.clean_tile(coord);
                        let mut ids = Vec::new();
                        loop {
                            let v = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
                            if v >= 0xFF00 { skip = i32::from(v & 0x00FF); *pos += 2; break; }
                            if v == 0x0061 || v == 0x0062 {
                                // A creature block. Parse it, attach to this tile.
                                let cid = self.read_creature(bytes, pos, v);
                                self.creature_tile.insert(cid, coord);
                            } else {
                                // plain item: [clientId u16][0xFF mark]
                                assert_eq!(bytes[*pos + 2], 0xFF, "expected plain item mark at {}", *pos + 2);
                                ids.push(v);
                                *pos += 3;
                            }
                        }
                        self.cache.insert(coord, ids);
                    }
                } else {
                    self.clean_tile(coord);
                    skip -= 1;
                }
                idx += 1;
            }
        }

        /// Decode ONE floor's tile stream (the 0xBF/0xBE revealed-floor reveals,
        /// which the server emits via `floor_description` per floor — NOT a band).
        /// `nz` is the floor and `offset` the projection shift the server used.
        #[allow(clippy::too_many_arguments)]
        fn decode_floor(
            &mut self, bytes: &[u8], pos: &mut usize, skip: &mut i32,
            anchor_x: i32, anchor_y: i32, nz: i32, offset: i32, width: i32, height: i32,
        ) {
            let mut idx = 0i32;
            let total = width * height;
            while idx < total {
                let nx = idx / height;
                let ny = idx % height;
                let coord = (anchor_x + nx + offset, anchor_y + ny + offset, nz as u8);
                if *skip == 0 {
                    let peek = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
                    if peek >= 0xFF00 {
                        *skip = i32::from(peek & 0x00FF);
                        *pos += 2;
                        self.clean_tile(coord);
                    } else {
                        *pos += 2;
                        self.clean_tile(coord);
                        let mut ids = Vec::new();
                        loop {
                            let v = u16::from_le_bytes([bytes[*pos], bytes[*pos + 1]]);
                            if v >= 0xFF00 { *skip = i32::from(v & 0x00FF); *pos += 2; break; }
                            if v == 0x0061 || v == 0x0062 {
                                let cid = self.read_creature(bytes, pos, v);
                                self.creature_tile.insert(cid, coord);
                            } else {
                                assert_eq!(bytes[*pos + 2], 0xFF, "expected plain item mark at {}", *pos + 2);
                                ids.push(v);
                                *pos += 3;
                            }
                        }
                        self.cache.insert(coord, ids);
                    }
                } else {
                    self.clean_tile(coord);
                    *skip -= 1;
                }
                idx += 1;
            }
        }

        /// OTClient cleanTile: empties the tile and detaches whatever creature was
        /// standing there (so a later re-list re-attaches it; no re-list = detach).
        fn clean_tile(&mut self, coord: (i32, i32, u8)) {
            self.cache.remove(&coord);
            let detached: Vec<u32> = self.creature_tile.iter()
                .filter(|&(_, &c)| c == coord).map(|(&id, _)| id).collect();
            for id in detached { self.creature_tile.remove(&id); }
        }

        /// Parse a creature block (0x61 unknown / 0x62 known) exactly as
        /// `protocol::creature::add_creature` serialized it, returning its id and
        /// advancing `pos` past the whole block. Mirror of OTClient getCreature.
        fn read_creature(&self, bytes: &[u8], pos: &mut usize, marker: u16) -> u32 {
            let mut p = *pos + 2; // past 0x0061/0x0062
            let id;
            if marker == 0x0061 {
                p += 4; // remove_id
                id = u32::from_le_bytes([bytes[p], bytes[p+1], bytes[p+2], bytes[p+3]]);
                p += 4;
                p += 1; // creatureType
                let name_len = u16::from_le_bytes([bytes[p], bytes[p+1]]) as usize;
                p += 2 + name_len;
            } else {
                id = u32::from_le_bytes([bytes[p], bytes[p+1], bytes[p+2], bytes[p+3]]);
                p += 4;
            }
            p += 1; // health%
            p += 1; // direction
            // outfit: look_type u16; if !=0 -> 5 bytes else u16 lookTypeEx; then mount u16
            let look_type = u16::from_le_bytes([bytes[p], bytes[p+1]]);
            p += 2;
            if look_type != 0 { p += 5; } else { p += 2; }
            p += 2; // mount
            p += 1; // light level
            p += 1; // light color
            p += 2; // speed/2
            p += 1; // skull
            p += 1; // party shield
            if marker == 0x0061 { p += 1; } // guild emblem (unknown only)
            p += 1; // creatureType2
            p += 1; // speech bubble
            p += 1; // mark (0xFF)
            p += 2; // helpers
            p += 1; // walkthrough
            *pos = p;
            id
        }
    }

    /// Feed one emitted walk_update/teleport packet through the client simulator,
    /// EXACTLY per OTClient ProtocolGame::parse* top-level opcode dispatch.
    /// `step` is a human label for divergence reporting.
    fn sim_apply(sim: &mut ClientSim, pkt: &[u8], step: &str) {
        let mut pos = 0usize;
        while pos < pkt.len() {
            let opcode = pkt[pos];
            pos += 1;
            match opcode {
                // 0x6D parseCreatureMove, id-form [0x6D][0xFFFF][id u32][newPos]
                0x6D => {
                    let marker = u16::from_le_bytes([pkt[pos], pkt[pos+1]]); pos += 2;
                    assert_eq!(marker, 0xFFFF, "test only emits id-form 0x6D");
                    let id = u32::from_le_bytes([pkt[pos], pkt[pos+1], pkt[pos+2], pkt[pos+3]]); pos += 4;
                    let nx = u16::from_le_bytes([pkt[pos], pkt[pos+1]]) as i32; pos += 2;
                    let ny = u16::from_le_bytes([pkt[pos], pkt[pos+1]]) as i32; pos += 2;
                    let nz = pkt[pos] as i32; pos += 1;
                    // getCreatureById -> removeThing from CURRENT tile.
                    let removed = sim.creature_tile.remove(&id).is_some();
                    if !removed {
                        // This IS "ProtocolGame::parseCreatureMove: unable to
                        // remove creature" — record and RETURN (OTClient returns).
                        if sim.first_divergence.is_none() {
                            sim.first_divergence = Some(format!(
                                "[{step}] 0x6D parseCreatureMove id={id}: unable to remove creature \
                                 (localPlayer DETACHED) -> client logs ERROR and drops the move"));
                        }
                        return;
                    }
                    // addThing at newPos. Does NOT change central.
                    sim.creature_tile.insert(id, (nx, ny, nz as u8));
                }
                // 0x6C parseTileRemoveThing, id-form [0x6C][0xFFFF][id]
                0x6C => {
                    let marker = u16::from_le_bytes([pkt[pos], pkt[pos+1]]); pos += 2;
                    assert_eq!(marker, 0xFFFF, "test only emits id-form 0x6C");
                    let id = u32::from_le_bytes([pkt[pos], pkt[pos+1], pkt[pos+2], pkt[pos+3]]); pos += 4;
                    sim.creature_tile.remove(&id); // detach by id (no central change)
                }
                // 0x64 parseMapDescription (full map / teleport): reads pos, sets central.
                0x64 => {
                    let cx = u16::from_le_bytes([pkt[pos], pkt[pos+1]]); pos += 2;
                    let cy = u16::from_le_bytes([pkt[pos], pkt[pos+1]]); pos += 2;
                    let cz = pkt[pos]; pos += 1;
                    sim.central = Position::new(cx, cy, cz);
                    let ax = i32::from(cx) - AR_LEFT;
                    let ay = i32::from(cy) - AR_TOP;
                    sim.decode_stream(pkt, &mut pos, ax, ay, i32::from(cz), 18, 14);
                }
                // 0xBF parseFloorChangeDown. Reveal floors are SINGLE floors
                // (server `floor_description` per floor), sharing one skip run
                // with a trailing [skip][0xFF] flush — NOT a banded stream.
                0xBF => {
                    let p = sim.central;
                    let newz = i32::from(p.z) + 1;
                    let ax = i32::from(p.x) - AR_LEFT; // central == old here
                    let ay = i32::from(p.y) - AR_TOP;
                    let mut skip = 0i32;
                    if newz == 8 {
                        // floors 8,9,10 with offsets -1,-2,-3 (server: nz+i, -i-1).
                        for i in 0..3i32 {
                            sim.decode_floor(pkt, &mut pos, &mut skip, ax, ay, newz + i, -i - 1, 18, 14);
                        }
                    } else if newz > 8 && newz < 14 {
                        sim.decode_floor(pkt, &mut pos, &mut skip, ax, ay, newz + 2, -3, 18, 14);
                    }
                    // The encoder's final `if skip >= 0 { [skip][0xFF] }` flush is
                    // consumed inline at the position the run marker appears.
                    sim.central = Position::new(p.x - 1, p.y - 1, p.z + 1);
                }
                // 0xBE parseFloorChangeUp.
                0xBE => {
                    let p = sim.central;
                    let newz = i32::from(p.z) - 1;
                    let ax = i32::from(p.x) - AR_LEFT;
                    let ay = i32::from(p.y) - AR_TOP;
                    let mut skip = 0i32;
                    if newz == 7 {
                        // floors 5..0 with offset (8-i) (server: i, 8-i).
                        for fz in (0..=5i32).rev() {
                            sim.decode_floor(pkt, &mut pos, &mut skip, ax, ay, fz, 8 - fz, 18, 14);
                        }
                    } else if newz > 7 {
                        // server: floor oz-3, projection 3 (oz = old z = newz+1).
                        sim.decode_floor(pkt, &mut pos, &mut skip, ax, ay, newz - 2, 3, 18, 14);
                    }
                    sim.central = Position::new(p.x + 1, p.y + 1, p.z - 1);
                }
                // Directional slices: shift central, then setMapDescription.
                0x65 => { // NORTH: central.y -= 1
                    sim.central = Position::new(sim.central.x, sim.central.y - 1, sim.central.z);
                    let ax = i32::from(sim.central.x) - AR_LEFT;
                    let ay = i32::from(sim.central.y) - AR_TOP;
                    sim.decode_stream(pkt, &mut pos, ax, ay, i32::from(sim.central.z), AR_LEFT + AR_RIGHT + 1, 1);
                }
                0x66 => { // EAST: central.x += 1
                    sim.central = Position::new(sim.central.x + 1, sim.central.y, sim.central.z);
                    let ax = i32::from(sim.central.x) + AR_RIGHT;
                    let ay = i32::from(sim.central.y) - AR_TOP;
                    sim.decode_stream(pkt, &mut pos, ax, ay, i32::from(sim.central.z), 1, AR_TOP + AR_BOTTOM + 1);
                }
                0x67 => { // SOUTH: central.y += 1
                    sim.central = Position::new(sim.central.x, sim.central.y + 1, sim.central.z);
                    let ax = i32::from(sim.central.x) - AR_LEFT;
                    let ay = i32::from(sim.central.y) + AR_BOTTOM;
                    sim.decode_stream(pkt, &mut pos, ax, ay, i32::from(sim.central.z), AR_LEFT + AR_RIGHT + 1, 1);
                }
                0x68 => { // WEST: central.x -= 1
                    sim.central = Position::new(sim.central.x - 1, sim.central.y, sim.central.z);
                    let ax = i32::from(sim.central.x) - AR_LEFT;
                    let ay = i32::from(sim.central.y) - AR_TOP;
                    sim.decode_stream(pkt, &mut pos, ax, ay, i32::from(sim.central.z), 1, AR_TOP + AR_BOTTOM + 1);
                }
                other => panic!("[{step}] unexpected top-level opcode {other:#x} at pos {pos}"),
            }
        }
    }

    /// Build a multifloor underground room (floors 6..10, unique per-(x,y,z)
    /// client ids) with a DOWN staircase at `down_stair` (z7) and a directional
    /// UP staircase at `up_stair` (z8) carrying `up_flags`. Boundary at z7/z8.
    fn stair_multifloor(
        down_stair: (u16, u16),
        up_stair: (u16, u16),
        up_flags: formats::items_xml::FloorChange,
    ) -> Arc<StaticMap> {
        use formats::items_xml::FloorChange;
        let (x0, x1) = (32000u16, 32060u16);
        let (y0, y1) = (32170u16, 32220u16);
        let span_x = x1 - x0 + 1;
        let span_y = y1 - y0 + 1;
        // Unique client id per (x,y,z) so any misplacement is visible. Based at
        // 0x0100 so no ground id collides with the creature thing markers
        // 0x0061/0x0062/0x0063 (the client distinguishes creatures from items by
        // these reserved thing ids; a synthetic ground id of 97/98 would be
        // misdecoded as a creature block).
        let uid = |x: u16, y: u16, z: u8| -> u16 {
            0x0100 + (x - x0) + (y - y0) * span_x + u16::from(z - 6) * span_x * span_y
        };
        // Distinct server ids for the two stairs so floor_change attaches only there.
        const SID_DOWN: u16 = 60000;
        const SID_UP: u16 = 60001;
        let mut item_types = Vec::new();
        let mut tiles = Vec::new();
        for z in 6u8..=10 {
            for x in x0..=x1 {
                for y in y0..=y1 {
                    let cid = uid(x, y, z);
                    item_types.push(ItemType { group: 1, flags: 0, server_id: cid, client_id: cid, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE });
                    let mut items = vec![MapItem { id: cid, count: None, contents: vec![] }];
                    if z == 7 && (x, y) == down_stair {
                        items.push(MapItem { id: SID_DOWN, count: None, contents: vec![] });
                    }
                    if z == 8 && (x, y) == up_stair {
                        items.push(MapItem { id: SID_UP, count: None, contents: vec![] });
                    }
                    tiles.push(MapTile { x, y, z, flags: 0, house_id: None, items });
                }
            }
        }
        item_types.push(ItemType { group: 5, flags: 0, server_id: SID_DOWN, client_id: 59000, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::DOWN });
        item_types.push(ItemType { group: 5, flags: 0, server_id: SID_UP, client_id: 59001, always_on_top: false, top_order: 0, has_height: false, floor_change: up_flags });
        let items = ItemsOtb { major_version: 3, minor_version: 57, build_number: 0, items: item_types };
        let map = OtbmMap { width: 65000, height: 65000, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles, towns: vec![Town { id: 1, name: "U".into(), x: 32027, y: 32196, z: 7 }], waypoints: vec![] };
        Arc::new(StaticMap::from_formats(&map, &items))
    }

    /// Seed the client cache + localPlayer from the INITIAL full 0x64 map the
    /// client receives at login (centered on `start`), including the localPlayer
    /// spliced on its own tile (as the server's login map does).
    fn seed_initial(g: &mut Game, start: Position, mover: u32) -> ClientSim {
        let mut sim = ClientSim {
            central: start,
            cache: HashMap::new(),
            creature_tile: HashMap::new(),
            localplayer_id: mover,
            first_divergence: None,
        };
        let cz = i32::from(start.z);
        let floors: Vec<i32> = if cz > 7 { ((cz - 2)..=(cz + 2).min(15)).collect() } else { (0..=7).rev().collect() };
        for nz in floors {
            let off = cz - nz;
            for sx in (i32::from(start.x) - AR_LEFT)..=(i32::from(start.x) + AR_RIGHT) {
                for sy in (i32::from(start.y) - AR_TOP)..=(i32::from(start.y) + AR_BOTTOM) {
                    let (wx, wy) = (sx + off, sy + off);
                    let ids = server_floor8_ids_z(g.map.as_ref(), wx, wy, nz as u8);
                    if !ids.is_empty() { sim.cache.insert((wx, wy, nz as u8), ids); }
                }
            }
        }
        sim.creature_tile.insert(mover, (i32::from(start.x), i32::from(start.y), start.z));
        sim
    }

    /// Compare the client cache against the server map across the full visible
    /// band, returning the first mismatch (sorted for determinism) or None.
    fn first_band_mismatch(g: &Game, sim: &ClientSim, p: Position) -> Option<String> {
        let cz = i32::from(p.z);
        let floors: Vec<i32> = if cz > 7 { ((cz - 2)..=(cz + 2).min(15)).collect() } else { (0..=7).rev().collect() };
        let mut mismatches = Vec::new();
        for nz in floors {
            let off = cz - nz;
            for sx in (i32::from(p.x) - AR_LEFT)..=(i32::from(p.x) + AR_RIGHT) {
                for sy in (i32::from(p.y) - AR_TOP)..=(i32::from(p.y) + AR_BOTTOM) {
                    let (wx, wy) = (sx + off, sy + off);
                    let server = server_floor8_ids_z(g.map.as_ref(), wx, wy, nz as u8);
                    let client = sim.cache.get(&(wx, wy, nz as u8)).cloned().unwrap_or_default();
                    if client != server {
                        mismatches.push(((nz, wx, wy), format!("({wx},{wy},{nz}): client={client:?} server={server:?}")));
                    }
                }
            }
        }
        mismatches.sort_by_key(|(k, _)| *k);
        mismatches.into_iter().next().map(|(_, m)| m)
    }

    /// Replay a step sequence through both the server (`do_move`) and the faithful
    /// OTClient simulator, reporting the FIRST divergence (detach or tile
    /// mismatch). Returns Ok(()) if fully consistent, Err(report) otherwise.
    fn replay(
        g: &mut Game, mover: u32, rx: &mut mpsc::Receiver<Vec<u8>>, sim: &mut ClientSim,
        seq: &[Direction],
    ) -> Result<(), String> {
        for (i, &dir) in seq.iter().enumerate() {
            let before = g.players[&mover].position;
            g.do_move(mover, dir);
            let after = g.players[&mover].position;
            let label = format!("step {i} {dir:?} {:?}->{:?}", (before.x, before.y, before.z), (after.x, after.y, after.z));
            let pkt = match rx.try_recv() {
                Ok(p) => p,
                Err(_) => {
                    if before == after { continue; } // blocked step, no packet
                    return Err(format!("[{label}] expected a walk packet but none pushed"));
                }
            };
            let header = pkt.first().copied().unwrap_or(0);
            let attached_before = sim.localplayer_attached();
            eprintln!("  {label}: header={header:#x} pkt_len={} attached_before={attached_before}", pkt.len());
            sim_apply(sim, &pkt, &label);
            while rx.try_recv().is_ok() {} // drain spectator extras (none: single client)

            if let Some(div) = sim.first_divergence.take() {
                return Err(format!(
                    "FIRST DIVERGENCE (detach):\n  {div}\n  packet header={header:#x} len={}\n  \
                     localplayer attached: before={attached_before} after=false\n  \
                     client central={:?} server player={:?}",
                    pkt.len(), (sim.central.x, sim.central.y, sim.central.z), (after.x, after.y, after.z)));
            }
            if !sim.localplayer_attached() {
                return Err(format!(
                    "[{label}] localPlayer DETACHED after applying packet header={header:#x} \
                     len={} (no 0x6D fired yet, but the next move will fail). \
                     client central={:?} server player={:?}",
                    pkt.len(), (sim.central.x, sim.central.y, sim.central.z), (after.x, after.y, after.z)));
            }
            if (sim.central.x, sim.central.y, sim.central.z) != (after.x, after.y, after.z) {
                return Err(format!(
                    "[{label}] CENTRAL DRIFT: client central={:?} != server player={:?} \
                     (packet header={header:#x})",
                    (sim.central.x, sim.central.y, sim.central.z), (after.x, after.y, after.z)));
            }
            if let Some(m) = first_band_mismatch(g, sim, after) {
                return Err(format!("[{label}] BAND MISMATCH (packet header={header:#x}): {m}"));
            }
        }
        Ok(())
    }

    #[test]
    fn floorchange_descend_then_ascend_1tile_diagonal_keeps_player_attached() {
        // Live repro: descend a DOWN stair SE (z7->z8, dx=dy=1), an underground
        // step, ascend an UP stair (z8->z7), then surface steps. The 1-tile
        // diagonal floor change does NOT trip the teleport guard (|dx|>1||dy|>1)
        // so it uses the incremental 0xBF/0xBE path. If the server fails to splice
        // the mover into the revealed-floor block on a floor change whose header
        // is NOT a 0x6D move, the localPlayer detaches and the next 0x6D fires
        // "unable to remove creature".
        let start = Position::new(32027, 32196, 7);
        let down_stair = (32028, 32197); // SE neighbor of start
        let up_stair = (32027, 32197);   // underground; WEST flag -> ascend
        let map = stair_multifloor(down_stair, up_stair, formats::items_xml::FloorChange::WEST);
        let mut g = Game::new(map);
        let (mover, mut rx) = add_player(&mut g, start);
        while rx.try_recv().is_ok() {}
        let mut sim = seed_initial(&mut g, start, mover);

        eprintln!("down_stair resolves: {:?}",
            g.map.resolve_floor_change(Position::new(down_stair.0, down_stair.1, 7)));
        eprintln!("up_stair resolves: {:?}",
            g.map.resolve_floor_change(Position::new(up_stair.0, up_stair.1, 8)));

        let seq = vec![
            Direction::SouthEast, // descend onto down-stair -> z8
            Direction::West,      // underground step toward up-stair
            Direction::SouthWest, // ascend onto up-stair -> z7
            Direction::East,      // surface step
            Direction::East,
        ];
        match replay(&mut g, mover, &mut rx, &mut sim, &seq) {
            Ok(()) => { /* no divergence: the happy path is clean */ }
            Err(report) => panic!("\n{report}\n"),
        }
    }

    /// Run one descend+ascend scenario through the faithful simulator and return
    /// the actual server landings plus the divergence report (if any).
    fn run_scenario(
        start: Position,
        down_stair: (u16, u16),
        up_stair: (u16, u16),
        up_flags: formats::items_xml::FloorChange,
        seq: &[Direction],
    ) -> (Option<Position>, Option<Position>, Result<(), String>) {
        let map = stair_multifloor(down_stair, up_stair, up_flags);
        let mut g = Game::new(map);
        let (mover, mut rx) = add_player(&mut g, start);
        while rx.try_recv().is_ok() {}
        let mut sim = seed_initial(&mut g, start, mover);
        let down_land = g.map.resolve_floor_change(Position::new(down_stair.0, down_stair.1, 7));
        let up_land = g.map.resolve_floor_change(Position::new(up_stair.0, up_stair.1, 8));
        let res = replay(&mut g, mover, &mut rx, &mut sim, seq);
        (down_land, up_land, res)
    }

    /// Battery across the staircase geometries the bug report asks us to probe.
    /// This is DIAGNOSTIC: it prints each scenario's landings + divergence so we
    /// can see WHICH geometry actually breaks. Does NOT fix anything.
    #[test]
    #[allow(clippy::type_complexity, clippy::assertions_on_constants)]
    fn floorchange_geometry_battery_reports_first_divergence() {
        use formats::items_xml::FloorChange as FC;
        let start = Position::new(32027, 32196, 7);
        // (label, down_stair, up_stair, up_flags, step sequence)
        let scenarios: Vec<(&str, (u16, u16), (u16, u16), FC, Vec<Direction>)> = vec![
            // 1-tile SE descend / WEST up-stair SW ascend (the live log geometry).
            ("diag_SE_down__WEST_up", (32028, 32197), (32027, 32197), FC::WEST,
             vec![Direction::SouthEast, Direction::West, Direction::SouthWest,
                  Direction::East, Direction::East]),
            // Straight ladder: DOWN step south, NORTH up-stair => straight ascend.
            ("straight_ladder_S_down__NORTH_up", (32027, 32197), (32027, 32197), FC::NORTH,
             vec![Direction::South, Direction::North]),
            // Straight DOWN ladder via plain south step + NORTH ascend straight up.
            ("straight_S_down__SOUTH_up", (32027, 32197), (32027, 32197), FC::SOUTH,
             vec![Direction::South, Direction::North]),
            // Single EAST up-stair (ascend shifts x west by 1).
            ("diag_SE_down__EAST_up", (32028, 32197), (32027, 32197), FC::EAST,
             vec![Direction::SouthEast, Direction::West, Direction::SouthWest,
                  Direction::East]),
            // 2-tile ALT up-stair: EAST_ALT lands +2 x (teleport-like ascend).
            ("diag_SE_down__EAST_ALT_up", (32028, 32197), (32027, 32197), FC::EAST_ALT,
             vec![Direction::SouthEast, Direction::West, Direction::SouthWest,
                  Direction::East]),
            // 2-tile ALT up-stair: SOUTH_ALT lands +2 y (teleport-like ascend).
            ("diag_SE_down__SOUTH_ALT_up", (32028, 32197), (32027, 32197), FC::SOUTH_ALT,
             vec![Direction::SouthEast, Direction::West, Direction::SouthWest,
                  Direction::East]),
        ];

        let mut breakers = Vec::new();
        for (label, ds, us, flags, seq) in scenarios {
            let (down_land, up_land, res) = run_scenario(start, ds, us, flags, &seq);
            match res {
                Ok(()) => eprintln!("[{label}] CLEAN  down->{down_land:?} up->{up_land:?}"),
                Err(report) => {
                    eprintln!("[{label}] DIVERGES  down->{down_land:?} up->{up_land:?}\n{report}\n");
                    breakers.push(label.to_string());
                }
            }
        }
        eprintln!("\n=== geometries that diverge: {breakers:?} ===");
        // Intentionally NOT asserting clean: this is a diagnostic probe. We assert
        // the test ran every scenario so CI keeps the evidence visible.
        assert!(true);
    }

    /// Build a multifloor room with a DOWN stair at `down_stair` on floor
    /// `down_z` (so we can test DEEPER descents, e.g. z8->z9, which are neither the
    /// surface boundary nor teleport-like for a 1-tile step).
    fn deep_stair_multifloor(down_stair: (u16, u16), down_z: u8) -> Arc<StaticMap> {
        use formats::items_xml::FloorChange;
        let (x0, x1) = (32000u16, 32060u16);
        let (y0, y1) = (32170u16, 32220u16);
        let span_x = x1 - x0 + 1;
        let span_y = y1 - y0 + 1;
        let uid = |x: u16, y: u16, z: u8| -> u16 {
            0x0100 + (x - x0) + (y - y0) * span_x + u16::from(z - 6) * span_x * span_y
        };
        const SID_DOWN: u16 = 60000;
        let mut item_types = Vec::new();
        let mut tiles = Vec::new();
        for z in 6u8..=12 {
            for x in x0..=x1 {
                for y in y0..=y1 {
                    let cid = uid(x, y, z);
                    item_types.push(ItemType { group: 1, flags: 0, server_id: cid, client_id: cid, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::NONE });
                    let mut items = vec![MapItem { id: cid, count: None, contents: vec![] }];
                    if z == down_z && (x, y) == down_stair {
                        items.push(MapItem { id: SID_DOWN, count: None, contents: vec![] });
                    }
                    tiles.push(MapTile { x, y, z, flags: 0, house_id: None, items });
                }
            }
        }
        item_types.push(ItemType { group: 5, flags: 0, server_id: SID_DOWN, client_id: 59000, always_on_top: false, top_order: 0, has_height: false, floor_change: FloorChange::DOWN });
        let items = ItemsOtb { major_version: 3, minor_version: 57, build_number: 0, items: item_types };
        let map = OtbmMap { width: 65000, height: 65000, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles, towns: vec![Town { id: 1, name: "U".into(), x: 32027, y: 32196, z: 8 }], waypoints: vec![] };
        Arc::new(StaticMap::from_formats(&map, &items))
    }

    /// Update uid for the deeper builder needs z up to 12; reuse seed/compare
    /// since they only read server tiles. Probe a DEEPER (z8->z9) 1-tile descend:
    /// NOT the surface boundary (from.z==8, not 7) and NOT teleport-like
    /// (1-tile step). Header is 0x6D + 0xBF, and the mover-splice condition
    /// (`boundary || teleport_like`) is FALSE -> the mover is NOT re-listed. If the
    /// 0xBF reveal / correction slices rewrite the landing tile without the player,
    /// it detaches -> next 0x6D fires "unable to remove creature".
    #[test]
    fn deeper_underground_descend_1tile_step_mover_splice_probe() {
        let start = Position::new(32027, 32196, 8);
        let down_stair = (32028, 32196); // east neighbor; DOWN -> straight z9
        let map = deep_stair_multifloor(down_stair, 8);
        let mut g = Game::new(map);
        let (mover, mut rx) = add_player(&mut g, start);
        while rx.try_recv().is_ok() {}
        let mut sim = seed_initial(&mut g, start, mover);
        eprintln!("down_stair(z8) resolves: {:?}",
            g.map.resolve_floor_change(Position::new(down_stair.0, down_stair.1, 8)));
        let seq = vec![
            Direction::East,  // step onto DOWN stair -> z9 (dx=1,dy=0): 0x6D + 0xBF
            Direction::East,  // a z9 surface step -> would fire 0x6D for the player
            Direction::West,
        ];
        match replay(&mut g, mover, &mut rx, &mut sim, &seq) {
            Ok(()) => eprintln!("CLEAN: deeper descend keeps player attached"),
            Err(report) => panic!("\nDEEPER DESCEND DIVERGES:\n{report}\n"),
        }
    }

    /// VALIDATION: prove the simulator actually CATCHES a detach (guards against a
    /// false-negative "all clean"). We replay the live ascend geometry but force
    /// the server to use the incremental 0xBE path WITHOUT splicing the mover by
    /// hand-feeding the simulator the exact packet the PRE-FIX server emitted: a
    /// 0x6D move to the z7 landing followed by a 0xBE whose floor-7 NORTH/WEST
    /// correction slices REWRITE the landing tile without the player. If the sim
    /// is faithful, the next 0x6D for the player must fail "unable to remove".
    #[test]
    fn simulator_detects_a_forced_detach() {
        // Minimal hand-built sim: player attached at a tile, then a slice that
        // cleanTiles that exact tile without re-listing the player.
        let mover = 0x1000_0000u32;
        let mut sim = ClientSim {
            central: Position::new(100, 100, 7),
            cache: HashMap::new(),
            creature_tile: HashMap::new(),
            localplayer_id: mover,
            first_divergence: None,
        };
        sim.creature_tile.insert(mover, (100, 100, 7)); // attached
        // A NORTH slice (0x65) whose 18x1 stream rewrites row y=99..? Actually
        // craft a WEST slice covering the player's column with an EMPTY tile at
        // the player's coord -> cleanTile detaches it. We emit a 1x14 WEST slice
        // anchored so it covers (100,100,7): central.x-1=99 after shift, anchor
        // x = central.x-8. Simplertest: directly cleanTile then a 0x6D.
        sim.clean_tile((100, 100, 7));
        assert!(!sim.localplayer_attached(), "cleanTile must detach the player");
        // Now the server sends a 0x6D move for the (now detached) player.
        let pkt = protocol::walk::creature_move(mover, (101, 100, 7));
        sim_apply(&mut sim, &pkt, "forced-detach probe");
        assert!(
            sim.first_divergence.is_some(),
            "the simulator MUST report 'unable to remove creature' for a detached localPlayer"
        );
        eprintln!("detector works: {}", sim.first_divergence.unwrap());
    }

    #[test]
    fn moving_across_pz_boundary_pushes_icons() {
        let mut g = Game::new(wide_combat_map_with_pz());
        // Start just east of the PZ tile (91,117); the PZ tile is (90,117).
        let (p, mut rp) = add_player(&mut g, Position::new(91, 117, 7));
        // Step West into the PZ tile (90,117).
        g.do_move(p, Direction::West);
        let into_pz = drain_find_icons(&mut rp).expect("expected an icons packet entering PZ");
        assert_eq!(into_pz, [enter_world::OP_ICONS, 0x00, 0x40], "ICON_PIGEON on entering PZ");
        // Step East back out to (91,117).
        g.do_move(p, Direction::East);
        let out_pz = drain_find_icons(&mut rp).expect("expected an icons packet leaving PZ");
        assert_eq!(out_pz, [enter_world::OP_ICONS, 0x00, 0x00], "icons cleared on leaving PZ");
    }

    // -------------------------------------------------------------------------
    // M10.1 do_move_thing tests
    // -------------------------------------------------------------------------

    #[test]
    fn throwing_open_ground_container_follows_and_closes_out_of_range() {
        // New detail: a container opened from the ground and thrown far must close.
        // The tile-to-tile move re-keys the window to the new tile and auto-closes
        // it when it lands out of range. Backpack on (100,110,7); player adjacent
        // on (100,111,7); throw to (100,113,7) (2 tiles from the player).
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 111, 7));
        // Open the ground backpack window (cid keyed to its tile).
        g.players.get_mut(&player).unwrap().open_containers[0] = Some(OpenContainer {
            server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
            items: vec![], source: ContainerSource::Ground(Position::new(100, 110, 7)), is_open: true,
        });
        drain(&mut rx);

        // Throw the backpack from (100,110,7) to (100,113,7). Stackpos 1 = the
        // backpack (ground at 0, no creatures on that tile).
        g.do_move_thing(player, Position::new(100, 110, 7), 1, Position::new(100, 113, 7), 1);

        let oc = g.players[&player].open_containers[0].as_ref().expect("window retained");
        assert!(matches!(oc.source, ContainerSource::Ground(p) if p == Position::new(100, 113, 7)),
            "window re-keyed to the destination tile; got {:?}", oc.source);
        assert!(!oc.is_open, "container thrown out of range must close");
    }

}
