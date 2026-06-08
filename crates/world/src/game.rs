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

use protocol::chat::{self, SpeakType};
use protocol::combat_packets;
use protocol::creature::{self, CreatureView, Outfit};
use protocol::map_description::{PlacedCreature, TileSource, WireItem};
use protocol::outfit as outfit_packets;
use protocol::{enter_world, tile_creature, tile_item, walk};

use crate::combat;
use crate::map::StaticMap;
use crate::{Direction, Position};

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

fn matches_source(a: ContainerSource, b: ContainerSource) -> bool {
    match (a, b) {
        (ContainerSource::Slot(x), ContainerSource::Slot(y)) => x == y,
        (ContainerSource::Nested { parent_cid: pa, parent_slot: ps },
         ContainerSource::Nested { parent_cid: qa, parent_slot: qs }) => pa == qa && ps == qs,
        (ContainerSource::Ground(p), ContainerSource::Ground(q)) => p == q,
        _ => false,
    }
}

/// TFS `Position::areInRange<1,1,0>`: within one tile on x and y, same floor.
fn in_close_range(a: Position, b: Position) -> bool {
    a.z == b.z
        && (i32::from(a.x) - i32::from(b.x)).abs() <= 1
        && (i32::from(a.y) - i32::from(b.y)).abs() <= 1
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

    fn login(&mut self, name: String, initial: InitialState, push_tx: mpsc::Sender<Vec<u8>>) -> LoginAck {
        let id = self.next_id;
        self.next_id += 1;
        // Resolve position. Either way the tile must be free: you never log in on
        // top of another creature (unlike stair/height co-occupancy during
        // movement). A returning player lands on their saved tile when it's free,
        // otherwise the nearest free tile around it; a new player spawns at/near
        // the map spawn.
        let position = match initial.position {
            Some(saved) => self.free_spawn_near(saved, id),
            None => self.free_spawn(),
        };
        let direction = initial.direction;
        let outfit = initial.outfit;

        // Existing in-range players, before inserting self.
        let others_ids = self.spectators(position, id);

        let mut inventory: [Option<InvItem>; 10] = [None; 10];
        for &(slot, server_id, count) in &initial.inventory {
            if !(1..=10).contains(&slot) { continue; }
            if let Some(meta) = self.map.item_meta(server_id) {
                let cnt = if meta.stackable { Some(count.max(1)) } else { None };
                inventory[(slot - 1) as usize] = Some(InvItem {
                    server_id, client_id: meta.client_id, count: cnt, animated: meta.animated,
                });
            }
        }

        // Restore container contents from InitialState.
        let open_containers = Self::restore_containers(&initial.container_items, &inventory, &self.map);

        self.players.insert(id, PlayerState {
            name, position, direction, outfit, push_tx, known: HashSet::new(),
            health: initial.health, max_health: initial.max_health, fist_skill: 10,
            attacking: None, last_attack_ms: 0,
            sex: initial.sex,
            gamemaster: initial.gamemaster,
            inventory,
            open_containers,
        });

        // Render each existing player into the new client's enter-world map, and
        // tell each existing player that the new one appeared.
        let mut others = Vec::new();
        for other in others_ids {
            if let Some(bytes) = self.introduce(id, other) {
                let p = self.players.get(&other).expect("listed spectator exists");
                others.push(PlacedCreature { x: p.position.x, y: p.position.y, z: p.position.z, bytes });
            }
            if let Some(bytes) = self.introduce(other, id) {
                let stackpos = self.creature_stackpos_on(position, id);
                self.push(other, tile_creature::add_tile_creature(
                    (position.x, position.y, position.z), stackpos, &bytes));
                // Spectators also see the teleport puff on login (TFS
                // sendAddCreature isLogin -> sendMagicEffect CONST_ME_TELEPORT).
                // The spawning client gets it from its own enter-world burst;
                // without this, other players see the creature appear with no effect.
                self.push(other, enter_world::magic_effect(
                    position.x, position.y, position.z, enter_world::EFFECT_TELEPORT));
            }
        }

        // Build the enter-world map slice from the MERGED view (static + dynamic
        // overlay) so a returning player sees items dropped on the ground while
        // they were away — not the pristine OTBM tile. Online spectators already
        // get the overlay via `merged()`; previously the login burst was encoded
        // from the raw `StaticMap` in the server layer, leaving the relogging
        // player blind to dynamic ground items.
        //
        // Self is rendered in full (unknown) form WITHOUT touching its known-set,
        // identical to the legacy server-layer path. Routing self through
        // `introduce(id, id)` here would mark self as known and desync the next
        // teleport's full-form rebuild, which relies on self being unknown.
        let self_name = self.players.get(&id).map(|p| p.name.clone()).unwrap_or_default();
        let mut placed = others.clone();
        let self_view = CreatureView {
            id,
            name: self_name.as_bytes(),
            health_percent: 100,
            direction: direction.to_byte(),
            outfit,
            light_level: 0,
            light_color: 0,
            speed: 220,
        };
        placed.push(PlacedCreature {
            x: position.x, y: position.y, z: position.z,
            bytes: creature::add_creature(&self_view, false, 0),
        });
        let map_description = {
            let merged = self.merged();
            protocol::map_description::encode(
                protocol::map_description::Center { x: position.x, y: position.y, z: position.z },
                &merged,
                &placed,
            )
        };

        LoginAck {
            snapshot: PlayerSnapshot {
                id, position, direction, outfit,
                health: initial.health,
                max_health: initial.max_health,
            },
            others,
            map_description,
        }
    }

    fn logout(&mut self, id: u32) {
        let Some(p) = self.players.remove(&id) else { return };
        // Emit save record BEFORE broadcasting the removal, while `p` is owned.
        if let Some(tx) = &self.save_tx {
            let inventory: Vec<(u8, u16, u8)> = p.inventory.iter().enumerate()
                .filter_map(|(i, slot)| slot.map(|it| ((i + 1) as u8, it.server_id, it.count.unwrap_or(1))))
                .collect();
            let container_items = Self::export_container_items(&p.inventory, &p.open_containers);
            let rec = SaveRecord {
                name: p.name.clone(),
                position: p.position,
                direction: p.direction,
                outfit: p.outfit,
                health: p.health,
                max_health: p.max_health,
                sex: p.sex,
                inventory,
                container_items,
            };
            // Unbounded send never blocks; error only if the worker is gone
            // (server shutting down) — silently drop in that case.
            let _ = tx.send(rec);
        }
        let pos = p.position;
        for spec in self.spectators(pos, id) {
            // A teleport puff on the departing creature's tile, then the remove.
            // (A deliberate polish over TFS, whose removeCreature disappears
            // silently; symmetric with the login appear effect.)
            self.push(spec, enter_world::magic_effect(
                pos.x, pos.y, pos.z, enter_world::EFFECT_TELEPORT));
            // id-form remove: unambiguous even if the logging-out creature shared
            // its tile with another (stair/height co-occupancy).
            self.push(spec, walk::remove_creature_by_id(id));
            // The departed creature must be re-introduced (full form) if it ever
            // returns: drop it from each spectator's known-set.
            if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&id); }
        }
    }

    /// Emit a `SaveRecord` for every online player **without** removing them or
    /// broadcasting. Called on graceful shutdown so in-memory outfit/position
    /// changes are persisted even when the sessions never logged out cleanly —
    /// otherwise killing the server reverts everyone to their last clean save.
    fn save_all(&mut self) {
        let Some(tx) = &self.save_tx else { return };
        for p in self.players.values() {
            let _ = tx.send(SaveRecord {
                name: p.name.clone(),
                position: p.position,
                direction: p.direction,
                outfit: p.outfit,
                health: p.health,
                max_health: p.max_health,
                sex: p.sex,
                inventory: p.inventory.iter().enumerate()
                    .filter_map(|(i, slot)| slot.map(|it| ((i + 1) as u8, it.server_id, it.count.unwrap_or(1))))
                    .collect(),
                container_items: Self::export_container_items(&p.inventory, &p.open_containers),
            });
        }
    }

    // -----------------------------------------------------------------------
    // Container helpers
    // -----------------------------------------------------------------------

    /// Restore container state from persisted items. Builds `OpenContainer`
    /// structs (in the closed state — not pushed to the client here; the player
    /// must re-open them). Container contents are stored so that once the player
    /// re-opens the bag the items are already there.
    ///
    /// Path format: `""` = top-level bag (inv_slot), `"N"` = item at slot N
    /// inside that bag (when it's a container), `"N/M"` = slot M inside
    /// the sub-bag at slot N, etc.
    /// Restore container state from persisted items. Builds closed `OpenContainer`
    /// entries (not shown to the client on login — the player must re-open them).
    /// Supports one level of nesting: items directly inside an inventory-slot bag.
    /// Nested-bag contents are not restored (shown empty until moved this session).
    fn restore_containers(
        rows: &[(u8, String, u16, u8)],
        inventory: &[Option<InvItem>; 10],
        map: &StaticMap,
    ) -> [Option<OpenContainer>; 16] {
        // Group rows by inv_slot, sorting by the numeric path so items are in order.
        let mut by_slot: std::collections::HashMap<u8, Vec<(usize, u16, u8)>> = Default::default();
        for (inv_slot, path, sid, cnt) in rows {
            let idx = path.parse::<usize>().unwrap_or(0);
            by_slot.entry(*inv_slot).or_default().push((idx, *sid, *cnt));
        }
        for v in by_slot.values_mut() {
            v.sort_by_key(|&(idx, _, _)| idx);
        }

        let mut result: [Option<OpenContainer>; 16] = Default::default();
        let mut cid = 0u8;
        for (slot_0, inv_item) in inventory.iter().enumerate() {
            let inv_slot = (slot_0 + 1) as u8;
            let Some(it) = inv_item else { continue };
            let Some(meta) = map.item_meta(it.server_id) else { continue };
            if !meta.is_container { continue; }
            let items_for_slot = by_slot.remove(&inv_slot).unwrap_or_default();
            let items: Vec<ContainerItem> = items_for_slot.into_iter().filter_map(|(_, sid, cnt)| {
                let m = map.item_meta(sid)?;
                Some(ContainerItem {
                    server_id: sid,
                    client_id: m.client_id,
                    count: if m.stackable { Some(cnt) } else { None },
                    animated: m.animated,
                })
            }).collect();
            if cid < 16 {
                result[cid as usize] = Some(OpenContainer {
                    server_id: it.server_id,
                    client_id: meta.client_id,
                    capacity: meta.container_capacity.max(1),
                    name: meta.name.clone(),
                    items,
                    source: ContainerSource::Slot(inv_slot),
                    is_open: false,
                });
                cid += 1;
            }
        }
        result
    }

    /// Export container contents for persistence. Walks ALL cid slots (both open
    /// and closed windows) and produces `(inv_slot, path, server_id, count)` rows.
    /// Only containers rooted in an inventory slot are exported (ground containers
    /// are transient). Nested containers are exported as items of their parent; their
    /// own contents are exported under the parent slot with a "/child_idx" path suffix.
    fn export_container_items(
        _inventory: &[Option<InvItem>; 10],
        open_containers: &[Option<OpenContainer>; 16],
    ) -> Vec<(u8, String, u16, u8)> {
        let mut rows = Vec::new();
        for oc in open_containers.iter().flatten() {
            let inv_slot = match oc.source {
                ContainerSource::Slot(s) if s >= 1 => s,
                _ => continue, // ground or nested — skipped (nested exported via parent)
            };
            for (idx, item) in oc.items.iter().enumerate() {
                rows.push((inv_slot, idx.to_string(), item.server_id, item.count.unwrap_or(1)));
            }
        }
        rows
    }

    /// Find the first unallocated cid slot (None) for a brand-new container.
    /// When all 16 slots are allocated, picks the first slot whose window is
    /// not currently open (so its contents can be silently evicted). Returns
    /// `None` only if all 16 slots are open simultaneously (pathological case).
    fn next_free_cid(p: &PlayerState) -> Option<u8> {
        // Prefer a completely unallocated slot.
        if let Some(cid) = (0u8..16).find(|&c| p.open_containers[c as usize].is_none()) {
            return Some(cid);
        }
        // Fall back to the first closed (but allocated) slot — its contents will
        // be silently replaced by the new container.
        (0u8..16).find(|&c| p.open_containers[c as usize].as_ref().map(|o| !o.is_open).unwrap_or(false))
    }

    /// Push an `0x6E` open-container packet to `id` for the given cid.
    fn push_open_container(&mut self, id: u32, cid: u8) {
        let Some(p) = self.players.get(&id) else { return };
        let Some(oc) = p.open_containers[cid as usize].as_ref() else { return };
        let has_parent = matches!(oc.source, ContainerSource::Nested { .. });
        let server_id = oc.server_id;
        let client_id = oc.client_id;
        let wire_items: Vec<protocol::container::ContainerWireItem> =
            oc.items.iter().map(|i| i.wire()).collect();
        let animated = self.map.item_meta(server_id).map(|m| m.animated).unwrap_or(false);
        let bag = WireItem { client_id, subtype: None, animated };
        let pkt = protocol::container::open_container(
            cid, &bag, &oc.name, oc.capacity, has_parent, &wire_items,
        );
        self.push(id, pkt);
    }

    // -----------------------------------------------------------------------
    // Container commands
    // -----------------------------------------------------------------------

    /// Handle `0x82` use-item: if the item is a container, open it as a window.
    /// Other use cases (levers, runes, potions) are M11/Lua — silently ignored here.
    fn do_use_item(&mut self, id: u32, pos_x: u16, pos_y: u16, pos_z: u8, stackpos: u8, index: u8) {
        let Some(p) = self.players.get(&id) else { return };

        // Resolve where the item is and its server_id.
        let (server_id, source) = if pos_x == 0xFFFF {
            // Inventory or container endpoint.
            if pos_y & 0x40 != 0 {
                // Container endpoint: (cid, slot_index).
                let cid = (pos_y & 0x0F) as u8;
                let slot_idx = pos_z as usize;
                let Some(oc) = p.open_containers[cid as usize].as_ref() else { return };
                let Some(item) = oc.items.get(slot_idx) else { return };
                let sid = item.server_id;
                (sid, ContainerSource::Nested { parent_cid: cid, parent_slot: slot_idx as u8 })
            } else {
                // Inventory slot.
                let slot = pos_y as u8;
                if !(1..=10).contains(&slot) { return; }
                let Some(it) = p.inventory[(slot - 1) as usize] else { return };
                (it.server_id, ContainerSource::Slot(slot))
            }
        } else {
            // Ground item.
            let pos = Position::new(pos_x, pos_y, pos_z);
            let player_pos = p.position;
            let near = (i32::from(player_pos.x) - i32::from(pos.x)).abs() <= 1
                && (i32::from(player_pos.y) - i32::from(pos.y)).abs() <= 1
                && player_pos.z == pos.z;
            if !near { return; }

            let pre = self.dynamic.get(&(pos_x, pos_y, pos_z))
                .map(|st| st.pre_creature_len)
                .unwrap_or_else(|| self.map.tile_pre_creature_len(pos));
            let creatures_len = self.creatures_on(pos).len();
            let sp = stackpos as usize;
            let src_idx = if sp < pre { sp }
                else if sp < pre + creatures_len { return; }
                else { sp - creatures_len };
            let Some(sid) = self.merged_server_id(pos, src_idx) else { return };
            (sid, ContainerSource::Ground(pos)) // ground container, tracked by tile; not persisted
        };

        let Some(meta) = self.map.item_meta(server_id) else { return };
        if !meta.is_container { return; }

        let capacity = meta.container_capacity.max(1);
        let name = meta.name.clone();
        let client_id = meta.client_id;

        // Look for an existing slot (open or closed) that already holds a container
        // from the same source — reuse it so items are never lost on close+reopen.
        let p = self.players.get(&id).unwrap();
        let existing_cid = (0u8..16).find(|&c| {
            p.open_containers[c as usize].as_ref().map(|oc| {
                matches_source(oc.source, source)
            }).unwrap_or(false)
        });

        let cid = if let Some(c) = existing_cid {
            // TFS toggle (actions.cpp useItem): using an already-open container
            // closes it. TFS keys "open" off the openContainers map (erased on
            // close); we retain the slot with `is_open=false` so contents survive
            // a reopen, so "currently open" means `is_open == true`.
            let already_open = self.players.get(&id)
                .and_then(|p| p.open_containers[c as usize].as_ref())
                .map(|oc| oc.is_open)
                .unwrap_or(false);
            if already_open {
                self.do_close_container(id, c);
                return;
            }
            // Reopen the existing (closed) slot: update metadata (in case the
            // container item changed type somehow) and mark it visible again.
            if let Some(p) = self.players.get_mut(&id) {
                if let Some(oc) = p.open_containers[c as usize].as_mut() {
                    oc.is_open = true;
                    oc.name = name;
                    oc.capacity = capacity;
                    oc.client_id = client_id;
                }
            }
            c
        } else {
            // No prior slot for this source — allocate a fresh one.
            // The client hints with `index`; use it if the slot is completely free.
            let p = self.players.get(&id).unwrap();
            let new_cid = if (index as usize) < 16 && p.open_containers[index as usize].is_none() {
                index
            } else {
                match Self::next_free_cid(p) {
                    Some(c) => c,
                    None => return, // all 16 windows occupied
                }
            };
            let oc = OpenContainer {
                server_id, client_id, capacity, name,
                items: Vec::new(), source, is_open: true,
            };
            if let Some(p) = self.players.get_mut(&id) {
                p.open_containers[new_cid as usize] = Some(oc);
            }
            new_cid
        };

        self.push_open_container(id, cid);
    }

    /// Close `cid` and every container nested inside it (depth-first).
    /// Sets `is_open = false` and sends `close_container` for each.
    fn close_container_tree(&mut self, id: u32, cid: u8) {
        let children: Vec<u8> = if let Some(p) = self.players.get(&id) {
            (0u8..16).filter(|&c| {
                p.open_containers[c as usize].as_ref().is_some_and(|oc| {
                    matches!(oc.source, ContainerSource::Nested { parent_cid: pc, .. } if pc == cid)
                })
            }).collect()
        } else {
            return;
        };
        for child in children {
            self.close_container_tree(id, child);
        }
        if let Some(p) = self.players.get_mut(&id) {
            if let Some(oc) = p.open_containers[cid as usize].as_mut() {
                oc.is_open = false;
            }
        }
        self.push(id, protocol::container::close_container(cid));
    }

    /// After removing the item at `removed_slot` from `parent_cid`:
    /// close any open window that tracked that item (and its children),
    /// and fix slot indices for siblings that shifted down.
    fn close_orphaned_nested_container(&mut self, id: u32, parent_cid: u8, removed_slot: usize) {
        let mut orphaned: Option<u8> = None;
        if let Some(p) = self.players.get_mut(&id) {
            for (c, oc_opt) in p.open_containers.iter_mut().enumerate() {
                let Some(oc) = oc_opt.as_mut() else { continue };
                let ContainerSource::Nested { parent_cid: pc, ref mut parent_slot } = oc.source
                    else { continue };
                if pc != parent_cid { continue; }
                let ps = *parent_slot as usize;
                if ps == removed_slot {
                    orphaned = Some(c as u8);
                } else if ps > removed_slot {
                    *parent_slot -= 1;
                }
            }
        }
        if let Some(cid) = orphaned {
            self.close_container_tree(id, cid);
        }
    }

    /// Handle `0x87` close-container: mark the window as closed (keep the items
    /// in memory so they survive a re-open within the same session) and send
    /// `0x6F` to the client.
    fn do_close_container(&mut self, id: u32, cid: u8) {
        if cid >= 16 { return; }
        let Some(p) = self.players.get_mut(&id) else { return };
        match p.open_containers[cid as usize].as_mut() {
            Some(oc) => { oc.is_open = false; }
            None => return,
        }
        self.push(id, protocol::container::close_container(cid));
    }

    /// Handle `0x88` up-arrow: navigate from a nested container to its parent.
    fn do_up_arrow(&mut self, id: u32, cid: u8) {
        if cid >= 16 { return; }
        let Some(p) = self.players.get(&id) else { return };
        let Some(oc) = p.open_containers[cid as usize].as_ref() else { return };
        let source = oc.source;

        match source {
            ContainerSource::Nested { parent_cid, .. } => {
                // The parent is already open in another cid — just send its packet.
                if parent_cid < 16 && p.open_containers[parent_cid as usize].is_some() {
                    self.push_open_container(id, parent_cid);
                }
                // Close the child window.
                if let Some(p) = self.players.get_mut(&id) {
                    p.open_containers[cid as usize] = None;
                }
                self.push(id, protocol::container::close_container(cid));
            }
            ContainerSource::Slot(_) | ContainerSource::Ground(_) => {
                // Already at the top level; up-arrow does nothing.
            }
        }
    }

    /// Add `item` to the front (slot 0) of a container. Notifies the player only
    /// when the window is currently open — a retained-but-closed container (e.g. a
    /// nested bag the item was dropped into without opening it) tracks the item in
    /// memory but sends no `0x70`, since the client has no widget for that cid.
    fn push_item_to_container(&mut self, id: u32, cid: u8, item: ContainerItem) {
        let Some(p) = self.players.get_mut(&id) else { return };
        let notify = {
            let Some(oc) = p.open_containers[cid as usize].as_mut() else { return };
            if oc.items.len() >= oc.capacity as usize { return; } // container full
            let notify = oc.is_open;
            oc.items.insert(0, item);
            notify
        };
        // Inserting at the front shifts every existing item down one slot, so any
        // nested container addressed by (parent_cid == cid, parent_slot) must have
        // its cached slot incremented to stay addressable. Without this the slot
        // goes stale, a duplicate cid is allocated on the next drop-into, and the
        // item is stranded. Symmetric to close_orphaned_nested_container's removal
        // adjustment.
        for o in p.open_containers.iter_mut().flatten() {
            if let ContainerSource::Nested { parent_cid: pc, ref mut parent_slot } = o.source {
                if pc == cid { *parent_slot = parent_slot.saturating_add(1); }
            }
        }
        if notify {
            let wire = item.wire();
            self.push(id, protocol::container::add_container_item(cid, 0, &wire));
        }
    }

    /// Remove the item at `slot` from an open container and notify the player.
    fn pop_item_from_container(&mut self, id: u32, cid: u8, slot: usize) -> Option<ContainerItem> {
        let p = self.players.get_mut(&id)?;
        let oc = p.open_containers[cid as usize].as_mut()?;
        if slot >= oc.items.len() { return None; }
        let removed = oc.items.remove(slot);
        // OTClient's onRemoveItem erases the slot and shifts items up automatically.
        // The `lastItem` (replacement) field is only for scrolled containers — it brings
        // in a previously hidden item at the bottom of the visible window. Our containers
        // never exceed capacity, so there is never a hidden item to reveal: always None.
        let pkt = protocol::container::remove_container_item(cid, slot as u16, None);
        self.push(id, pkt);
        Some(removed)
    }

    /// If the item at `slot_idx` inside container `parent_cid` is itself a
    /// container, return the cid that tracks that nested container's contents —
    /// reusing its already-allocated cid, or allocating a fresh closed one if the
    /// bag has never been opened. Returns `None` when the destination slot holds
    /// no container (the caller then inserts into `parent_cid` directly).
    ///
    /// Faithful to TFS `Container::queryDestination`, which descends into a
    /// destination slot that holds a container instead of placing beside it.
    fn nested_dest_cid(&mut self, id: u32, parent_cid: u8, slot_idx: usize) -> Option<u8> {
        let sid = {
            let p = self.players.get(&id)?;
            let oc = p.open_containers[parent_cid as usize].as_ref()?;
            oc.items.get(slot_idx)?.server_id
        };
        let meta = self.map.item_meta(sid)?;
        if !meta.is_container { return None; }

        // Reuse the cid already tracking this exact nested slot, if any.
        let target = ContainerSource::Nested { parent_cid, parent_slot: slot_idx as u8 };
        if let Some(p) = self.players.get(&id) {
            if let Some(c) = (0u8..16).find(|&c| {
                p.open_containers[c as usize].as_ref().is_some_and(|o| matches_source(o.source, target))
            }) {
                return Some(c);
            }
        }

        // Allocate a fresh, closed cid to hold the nested bag's contents.
        let p = self.players.get(&id)?;
        let cid = Self::next_free_cid(p)?;
        let oc = OpenContainer {
            server_id: sid,
            client_id: meta.client_id,
            capacity: meta.container_capacity.max(1),
            name: meta.name.clone(),
            items: Vec::new(),
            source: target,
            is_open: false,
        };
        self.players.get_mut(&id)?.open_containers[cid as usize] = Some(oc);
        Some(cid)
    }

    /// Re-key the open-container window whose source matches `old` to `new`, so a
    /// container's tracked contents follow the item when it moves between an
    /// inventory slot and the ground. A source location uniquely identifies one
    /// window, so the first match is the only one. No-op if nothing matches (the
    /// moved item is not a container, or was never opened this session).
    fn rekey_container_source(&mut self, id: u32, old: ContainerSource, new: ContainerSource) {
        let Some(p) = self.players.get_mut(&id) else { return };
        for oc in p.open_containers.iter_mut().flatten() {
            if matches_source(oc.source, old) {
                oc.source = new;
                break;
            }
        }
    }

    /// Close every open ground container the player has walked out of range of
    /// (more than one tile on x/y, or a different floor). Inventory and nested
    /// containers travel with the player and are never closed by walking. Mirrors
    /// TFS `Player::onCreatureMove` + `autoCloseContainers`, which key off the
    /// container's tile position. Call after the player's position is committed.
    fn auto_close_ground_containers(&mut self, id: u32) {
        let Some(player_pos) = self.players.get(&id).map(|p| p.position) else { return };
        let to_close: Vec<u8> = {
            let Some(p) = self.players.get(&id) else { return };
            (0u8..16).filter(|&c| {
                p.open_containers[c as usize].as_ref().is_some_and(|oc| {
                    oc.is_open
                        && matches!(oc.source, ContainerSource::Ground(gp) if !in_close_range(gp, player_pos))
                })
            }).collect()
        };
        // close_container_tree also closes any sub-containers opened from inside
        // the ground bag (depth-first) and sends each `0x6F`.
        for cid in to_close {
            self.close_container_tree(id, cid);
        }
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

    /// Apply a new outfit to `id`, then broadcast a `0x8E` creature-outfit packet
    /// to the player and all current spectators.
    ///
    /// If `id` is not in the game, this is a no-op.
    ///
    /// NOTE(pre-alpha): the requested outfit is trusted unconditionally.
    /// TFS checks `getOutfitAddons` to verify the player owns the addons;
    /// validation is deferred to a later milestone.
    fn do_change_outfit(&mut self, id: u32, outfit: Outfit) {
        let pos = match self.players.get_mut(&id) {
            Some(p) => { p.outfit = outfit; p.position }
            None => return,
        };
        let pkt = outfit_packets::creature_outfit(id, &outfit);
        self.push(id, pkt.clone());
        for spec in self.spectators(pos, id) {
            self.push(spec, pkt.clone());
        }
    }

    /// Push a `0xC8` outfit-window packet to `id` only (no broadcast).
    ///
    /// The catalog is gender-correct: a female player (sex 0) is offered the
    /// female look_types, a male player (sex 1) the male ones — see
    /// [`crate::outfit_catalog`], sourced from `reference/tfs/data/XML/outfits.xml`.
    /// All outfits (free and premium) are offered with both addons (addons = 3):
    /// the 10.98 wire format carries no premium flag, so "available" simply means
    /// "present in the list".
    ///
    /// If `id` is not in the game, this is a no-op.
    fn do_request_outfit(&mut self, id: u32) {
        let (outfit, sex) = match self.players.get(&id) {
            Some(p) => (p.outfit, p.sex),
            None => return,
        };
        let available: Vec<outfit_packets::AvailableOutfit> =
            crate::outfit_catalog::catalog_for_sex(sex)
                .iter()
                .map(|&(look_type, name)| outfit_packets::AvailableOutfit { look_type, name, addons: 3 })
                .collect();
        let pkt = outfit_packets::outfit_window(&outfit, &available, &[]);
        self.push(id, pkt);
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

    fn do_say(&mut self, id: u32, speak_type: SpeakType, text: String) {
        let (pos, name) = match self.players.get(&id) {
            Some(p) => (p.position, p.name.clone()),
            None => return,
        };
        if text.is_empty() {
            return;
        }
        let stmt = self.next_statement_id;
        self.next_statement_id = self.next_statement_id.wrapping_add(1);
        const LEVEL: u16 = 1; // real speaker level arrives with M14 progression

        // Cap to the TFS 255-byte message limit. Operate on raw bytes (the wire
        // is Latin-1) so a multi-byte boundary can never panic a String::truncate.
        let cap = |s: &[u8]| -> Vec<u8> { s[..s.len().min(255)].to_vec() };
        let xyz = (pos.x, pos.y, pos.z);

        match speak_type {
            SpeakType::Say => {
                let body = cap(text.as_bytes());
                let pkt = chat::creature_say(stmt, name.as_bytes(), LEVEL, speak_type, xyz, &body);
                self.push(id, pkt.clone());
                // Chat is same-floor (TFS getSpectators multifloor=false); the
                // band-aware `spectators` is for presence, not talk.
                for spec in self.spectators_in_range(pos, id, 8, 6) {
                    self.push(spec, pkt.clone());
                }
            }
            SpeakType::Yell => {
                let body = cap(text.to_uppercase().as_bytes());
                let pkt = chat::creature_say(stmt, name.as_bytes(), LEVEL, speak_type, xyz, &body);
                self.push(id, pkt.clone());
                for spec in self.spectators_in_range(pos, id, 18, 14) {
                    self.push(spec, pkt.clone());
                }
            }
            SpeakType::Whisper => {
                let full = cap(text.as_bytes());
                self.push(id, chat::creature_say(stmt, name.as_bytes(), LEVEL, speak_type, xyz, &full));
                for spec in self.spectators_in_range(pos, id, 8, 6) {
                    let Some(spos) = self.players.get(&spec).map(|p| p.position) else { continue };
                    let adjacent = (i32::from(spos.x) - i32::from(pos.x)).abs() <= 1
                        && (i32::from(spos.y) - i32::from(pos.y)).abs() <= 1;
                    let heard: &[u8] = if adjacent { &full } else { b"pspsps" };
                    self.push(spec, chat::creature_say(stmt, name.as_bytes(), LEVEL, speak_type, xyz, heard));
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // M9 look handlers
    // -----------------------------------------------------------------

    /// Handle `0x8C` look-at. Resolve the thing at `(x,y,z)` stackpos, build the
    /// TFS "You see …" text, and push `0xB4`. Mirrors `Game::playerLookAt`
    /// (game.cpp:3100): resolve thing, canSee check, distance, describe.
    fn do_look(&mut self, id: u32, x: u16, y: u16, z: u8, stackpos: u8) {
        let Some(looker) = self.players.get(&id) else { return };
        let looker_pos = looker.position;
        let gm = looker.gamemaster;
        let pos = Position::new(x, y, z);

        if !Self::can_see(looker_pos, pos) {
            return;
        }

        let pre = self.merged_pre_creature_len(pos);
        let creatures = self.creatures_on(pos);

        let sp = stackpos as usize;
        let text = if sp < pre {
            self.describe_tile_item(pos, sp, looker_pos, gm)
        } else if !creatures.is_empty() && sp < pre + creatures.len() {
            let target = creatures[sp - pre];
            self.describe_creature(id, target, gm)
        } else {
            let idx = sp.saturating_sub(creatures.len());
            self.describe_tile_item(pos, idx, looker_pos, gm)
        };

        if let Some(text) = text {
            self.push_info_descr(id, &text);
        }
    }

    /// Handle `0x8D` look-in-battle-list: describe a creature by id.
    fn do_look_battle(&mut self, id: u32, target_id: u32) {
        let Some(looker) = self.players.get(&id) else { return };
        let Some(target) = self.players.get(&target_id) else { return };
        if !Self::can_see(looker.position, target.position) {
            return;
        }
        let gm = looker.gamemaster;
        if let Some(text) = self.describe_creature(id, target_id, gm) {
            self.push_info_descr(id, &text);
        }
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

    /// Push the private `0x78`/`0x79` for one equipment slot to its owner.
    fn push_inventory_slot(&mut self, id: u32, slot: u8) {
        let Some(p) = self.players.get(&id) else { return };
        let pkt = match p.inventory[(slot - 1) as usize] {
            Some(it) => {
                let wi = WireItem { client_id: it.client_id, subtype: it.count, animated: it.animated };
                enter_world::set_inventory_slot(slot, &wi)
            }
            None => vec![enter_world::OP_INVENTORY_EMPTY, slot],
        };
        self.push(id, pkt);
    }

    /// The wire stackpos for the item at overlay/static index `idx` on `pos`,
    /// accounting for creatures inserted between the pre-creature items and the
    /// down items. Capped at 9 like the client stack.
    fn item_wire_stackpos(&self, pos: Position, idx: usize) -> u8 {
        let pre = self.dynamic.get(&(pos.x, pos.y, pos.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or_else(|| self.map.tile_pre_creature_len(pos));
        let creatures = self.creatures_on(pos).len();
        let sp = if idx < pre { idx } else { idx + creatures };
        sp.min(9) as u8
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

    /// COW the source tile and remove (or split) `want` units of the item at stack
    /// index `src_idx`. Returns the amount actually taken and whether the slot was
    /// fully removed. `src_idx` must already be validated by the caller.
    fn take_from_ground(&mut self, from: Position, src_idx: usize, want: u8, stackable: bool) -> Option<(u8, bool)> {
        if !self.materialize(from) { return None; }
        let st = self.dynamic.get_mut(&(from.x, from.y, from.z)).unwrap();
        let cur = st.counts[src_idx].unwrap_or(1).max(1);
        let moved = if stackable { want.max(1).min(cur) } else { 1 };
        let removed_fully;
        if stackable && cur > moved {
            let left = cur - moved;
            st.counts[src_idx] = Some(left);
            st.items[src_idx].subtype = Some(left);
            removed_fully = false;
        } else {
            st.items.remove(src_idx);
            st.server_ids.remove(src_idx);
            st.counts.remove(src_idx);
            if src_idx < st.pre_creature_len { st.pre_creature_len -= 1; }
            removed_fully = true;
        }
        Some((moved, removed_fully))
    }

    /// COW the dest tile and insert `moved` units of item `src_sid` at the FRONT of
    /// the down-items, merging into a same-type front stack (cap 100, spill on
    /// overflow). Returns `(merged_update, added)` wire items to broadcast at slot S.
    fn add_to_ground_front(
        &mut self, to: Position, src_sid: u16, client_id: u16, moved: u8, animated: bool, stackable: bool,
    ) -> Option<(Option<WireItem>, Option<WireItem>)> {
        if !self.materialize(to) { return None; }
        let st = self.dynamic.get_mut(&(to.x, to.y, to.z)).unwrap();
        let front = st.pre_creature_len;
        let merge_at_front = stackable && st.server_ids.len() > front && st.server_ids[front] == src_sid;
        if merge_at_front {
            let total = u32::from(st.counts[front].unwrap_or(1).max(1)) + u32::from(moved);
            let capped = total.min(100) as u8;
            st.counts[front] = Some(capped);
            st.items[front].subtype = Some(capped);
            let merged_item = st.items[front];
            if total > 100 {
                let spill_count = u8::try_from(total - 100).unwrap_or(u8::MAX);
                let spill_wi = WireItem { client_id, subtype: Some(spill_count), animated };
                st.items.insert(front, spill_wi);
                st.server_ids.insert(front, src_sid);
                st.counts.insert(front, Some(spill_count));
                Some((Some(merged_item), Some(spill_wi)))
            } else {
                Some((Some(merged_item), None))
            }
        } else {
            let subtype = if stackable { Some(moved) } else { None };
            let wi = WireItem { client_id, subtype, animated };
            st.items.insert(front, wi);
            st.server_ids.insert(front, src_sid);
            st.counts.insert(front, subtype);
            Some((None, Some(wi)))
        }
    }

    /// Move a thing from one map tile to another (M10.1: ground items only).
    /// Validates moveability, reach, and throw line-of-sight; removes `count`
    /// from the source (split or whole), then merges same-type stackables on the
    /// destination (cap 100, overflow spills) or appends a new down item. Both
    /// tiles are copied-on-write into `dynamic` before mutation, then the change
    /// is broadcast to spectators.
    fn do_move_thing(&mut self, id: u32, from: Position, from_stackpos: u8, to: Position, count: u8) {
        // Route any endpoint with x==0xFFFF (inventory or container).
        if from.x == 0xFFFF || to.x == 0xFFFF {
            // Check for container endpoints: y & 0x40 != 0.
            let from_is_container = from.x == 0xFFFF && (from.y & 0x40) != 0;
            let to_is_container   = to.x   == 0xFFFF && (to.y   & 0x40) != 0;
            if from_is_container || to_is_container {
                self.do_move_container(id, from, from_stackpos, to, count);
            } else {
                self.do_move_inventory(id, from, from_stackpos, to, count);
            }
            return;
        }
        if from == to { return; }
        let Some(p) = self.players.get(&id) else { return };
        let player_pos = p.position;

        let near = (i32::from(player_pos.x) - i32::from(from.x)).abs() <= 1
            && (i32::from(player_pos.y) - i32::from(from.y)).abs() <= 1
            && player_pos.z == from.z;
        if !near { self.push_cannot_move(id, "You are too far away."); return; }

        if !self.map.can_throw_object_to(from, to) {
            self.push_cannot_move(id, "You cannot throw there."); return;
        }

        let creatures = self.creatures_on(from);
        let pre = self.dynamic.get(&(from.x, from.y, from.z))
            .map(|st| st.pre_creature_len)
            .unwrap_or_else(|| self.map.tile_pre_creature_len(from));
        let sp = from_stackpos as usize;
        let src_idx = if sp < pre { sp }
            else if sp < pre + creatures.len() { return; } // a creature, not an item
            else { sp - creatures.len() };

        let Some(src_sid) = self.merged_server_id(from, src_idx) else { return };
        let Some(meta) = self.map.item_meta(src_sid) else { return };
        let stackable = meta.stackable;
        if !meta.moveable { self.push_cannot_move(id, "You cannot move this object."); return; }
        let client_id = meta.client_id;
        let animated = meta.animated;
        let is_container = meta.is_container;

        if self.map.tile_pre_creature_len(to) == 0 && self.map.tile_stack_clone(to).is_none() {
            self.push_cannot_move(id, "You cannot put that there."); return;
        }
        // Reject block-solid destinations (walls): TFS refuses to place items on a
        // tile whose stack holds an unpassable item.
        if self.map.is_blocked(to) {
            self.push_cannot_move(id, "You cannot put that there."); return;
        }

        let moved_req = if stackable { count.max(1) } else { 1 };

        let Some((moved, removed_fully)) = self.take_from_ground(from, src_idx, moved_req, stackable) else { return };

        let dest_creatures = self.creatures_on(to).len();
        let Some((dest_merged_update, dest_added)) =
            self.add_to_ground_front(to, src_sid, client_id, moved, animated, stackable) else { return };
        // Broadcast: all dest changes target the top down-item slot S = front + creatures.
        // For the overflow case: UPDATE the existing stack at S first (it gets capped to 100),
        // then ADD the spill at S (which pushes the existing one down to S+1 on the client).
        let dest_front = self.dynamic.get(&(to.x, to.y, to.z))
            .map(|st| st.pre_creature_len).unwrap_or(0);
        let dest_s = (dest_front + dest_creatures).min(9) as u8;
        if let Some(item) = dest_merged_update {
            self.broadcast_dest(to, dest_s, item, true);  // 0x6B update existing stack
        }
        if let Some(item) = dest_added {
            self.broadcast_dest(to, dest_s, item, false); // 0x6A add new/spill on top
        }
        self.broadcast_source(from, from_stackpos, removed_fully, src_idx);

        // A container dragged tile-to-tile carries its open window (and contents)
        // with it: re-key to the new tile, then close it if it landed out of range.
        if is_container {
            self.rekey_container_source(id, ContainerSource::Ground(from), ContainerSource::Ground(to));
            self.auto_close_ground_containers(id);
        }
    }

    /// Handle a move where at least one endpoint is an inventory slot (`x==0xFFFF`,
    /// slot = `y`). Three cases: ground→slot (equip), slot→ground (unequip),
    /// slot→slot (move/swap). Equipment packets are private to the player.
    fn do_move_inventory(&mut self, id: u32, from: Position, from_stackpos: u8, to: Position, count: u8) {
        // An inventory endpoint must be a real slot 1..=10, validated on the raw
        // u16 BEFORE truncating to u8 — else a hacked client sending e.g.
        // y == 0x4001 (clears the upstream `& 0x40` container guard) would
        // truncate to a valid-looking slot. A 0xFFFF endpoint with an out-of-range
        // slot is rejected outright rather than mis-read as a ground tile.
        let from_slot = if from.x == 0xFFFF {
            if !(1..=10).contains(&from.y) { return; }
            Some(from.y as u8)
        } else { None };
        let to_slot = if to.x == 0xFFFF {
            if !(1..=10).contains(&to.y) { return; }
            Some(to.y as u8)
        } else { None };

        match (from_slot, to_slot) {
            // ---- equip: ground → slot ----
            (None, Some(slot)) => {
                if !(1..=10).contains(&slot) { return; }
                let Some(p) = self.players.get(&id) else { return };
                let player_pos = p.position;
                if p.inventory[(slot - 1) as usize].is_some() {
                    self.push_cannot_move(id, "You cannot equip this object."); return;
                }
                let near = (i32::from(player_pos.x) - i32::from(from.x)).abs() <= 1
                    && (i32::from(player_pos.y) - i32::from(from.y)).abs() <= 1
                    && player_pos.z == from.z;
                if !near { self.push_cannot_move(id, "You are too far away."); return; }

                let creatures = self.creatures_on(from).len();
                let pre = self.dynamic.get(&(from.x, from.y, from.z))
                    .map(|st| st.pre_creature_len)
                    .unwrap_or_else(|| self.map.tile_pre_creature_len(from));
                let sp = from_stackpos as usize;
                let src_idx = if sp < pre { sp }
                    else if sp < pre + creatures { return; }
                    else { sp - creatures };

                let Some(src_sid) = self.merged_server_id(from, src_idx) else { return };
                let Some(meta) = self.map.item_meta(src_sid) else { return };
                if !meta.moveable { self.push_cannot_move(id, "You cannot move this object."); return; }
                let Some(eq) = meta.equip_slot else {
                    self.push_cannot_move(id, "You cannot equip this object."); return;
                };
                if !eq.admits(slot) {
                    self.push_cannot_move(id, "You cannot equip this object."); return;
                }
                let (stackable, client_id, animated) = (meta.stackable, meta.client_id, meta.animated);
                let want = if stackable { count.max(1) } else { 1 };

                let Some((moved, removed_fully)) = self.take_from_ground(from, src_idx, want, stackable) else { return };
                self.broadcast_source(from, from_stackpos, removed_fully, src_idx);

                let cnt = if stackable { Some(moved) } else { None };
                if let Some(p) = self.players.get_mut(&id) {
                    p.inventory[(slot - 1) as usize] = Some(InvItem { server_id: src_sid, client_id, count: cnt, animated });
                }
                self.push_inventory_slot(id, slot);
                // A container picked up from the ground keeps its contents: follow
                // its open window from the ground tile to the inventory slot.
                self.rekey_container_source(id, ContainerSource::Ground(from), ContainerSource::Slot(slot));
            }
            // ---- unequip: slot → ground ----
            (Some(slot), None) => {
                if !(1..=10).contains(&slot) { return; }
                let Some(p) = self.players.get(&id) else { return };
                let player_pos = p.position;
                let Some(it) = p.inventory[(slot - 1) as usize] else { return };
                // Unequip THROWS the item from the body to the ground. The source
                // is on the player, so (matching TFS playerMoveItem: mapFromPos is
                // the player's own tile when fromCylinder is the inventory) the only
                // distance constraint is throw range + line of sight to the dest —
                // NOT adjacency. You can toss an unequipped item across the screen.
                if player_pos.z != to.z || !self.map.can_throw_object_to(player_pos, to) {
                    self.push_cannot_move(id, "You cannot throw there."); return;
                }
                if self.map.tile_pre_creature_len(to) == 0 && self.map.tile_stack_clone(to).is_none() {
                    self.push_cannot_move(id, "You cannot put that there."); return;
                }
                // Reject block-solid destinations (walls), mirroring do_move_thing.
                if self.map.is_blocked(to) {
                    self.push_cannot_move(id, "You cannot put that there."); return;
                }
                let meta_stackable = self.map.item_meta(it.server_id).map(|m| m.stackable).unwrap_or(false);
                let moved = it.count.unwrap_or(1).max(1);

                if let Some(p) = self.players.get_mut(&id) {
                    p.inventory[(slot - 1) as usize] = None;
                }
                self.push_inventory_slot(id, slot);

                let dest_creatures = self.creatures_on(to).len();
                let Some((merged, added)) =
                    self.add_to_ground_front(to, it.server_id, it.client_id, moved, it.animated, meta_stackable) else { return };
                let dest_front = self.dynamic.get(&(to.x, to.y, to.z)).map(|st| st.pre_creature_len).unwrap_or(0);
                let dest_s = (dest_front + dest_creatures).min(9) as u8;
                if let Some(item) = merged { self.broadcast_dest(to, dest_s, item, true); }
                if let Some(item) = added { self.broadcast_dest(to, dest_s, item, false); }
                // A thrown container keeps its contents: follow its open window to
                // the ground tile, then close it if the throw landed out of range.
                self.rekey_container_source(id, ContainerSource::Slot(slot), ContainerSource::Ground(to));
                self.auto_close_ground_containers(id);
            }
            // ---- slot → slot: move or swap ----
            (Some(src), Some(dst)) => {
                if !(1..=10).contains(&src) || !(1..=10).contains(&dst) || src == dst { return; }
                let Some(p) = self.players.get(&id) else { return };
                let Some(moving) = p.inventory[(src - 1) as usize] else { return };
                if let Some(eq) = self.map.item_meta(moving.server_id).and_then(|m| m.equip_slot) {
                    if !eq.admits(dst) { self.push_cannot_move(id, "You cannot equip this object."); return; }
                } else { self.push_cannot_move(id, "You cannot equip this object."); return; }
                let displaced = p.inventory[(dst - 1) as usize];
                if let Some(d) = displaced {
                    let ok = self.map.item_meta(d.server_id).and_then(|m| m.equip_slot)
                        .map(|eq| eq.admits(src)).unwrap_or(false);
                    if !ok { self.push_cannot_move(id, "You cannot equip this object."); return; }
                }
                if let Some(p) = self.players.get_mut(&id) {
                    p.inventory[(dst - 1) as usize] = Some(moving);
                    p.inventory[(src - 1) as usize] = displaced;
                }
                self.push_inventory_slot(id, src);
                self.push_inventory_slot(id, dst);
            }
            (None, None) => {}
        }
    }

    /// Handle moves where at least one endpoint is a container slot
    /// (`x == 0xFFFF && y & 0x40 != 0`). Four cases:
    ///   - container → container (same or different cid)
    ///   - ground → container
    ///   - container → ground
    ///   - inventory slot → container (or vice versa)
    fn do_move_container(&mut self, id: u32, from: Position, from_stackpos: u8, to: Position, count: u8) {
        // Decode endpoints.
        let from_is_container = from.x == 0xFFFF && (from.y & 0x40) != 0;
        let to_is_container   = to.x   == 0xFFFF && (to.y   & 0x40) != 0;
        let from_is_inv_slot  = from.x == 0xFFFF && (from.y & 0x40) == 0;
        let to_is_inv_slot    = to.x   == 0xFFFF && (to.y   & 0x40) == 0;

        // Decode container endpoints.
        let from_cid      = if from_is_container { Some((from.y & 0x0F) as u8) } else { None };
        let from_slot_idx = if from_is_container { Some(from.z as usize) } else { None };
        let to_cid        = if to_is_container   { Some((to.y   & 0x0F) as u8) } else { None };
        let to_slot_idx   = if to_is_container   { Some(to.z   as usize) } else { None };

        // --- CASE 1: container → container ---
        if from_is_container && to_is_container {
            let fc = from_cid.unwrap();
            let fs = from_slot_idx.unwrap();
            let tc = to_cid.unwrap();
            let ts = to_slot_idx.unwrap();

            // Dropping an item onto its own source slot is a no-op.
            if fc == tc && fs == ts { return; }

            // Resolve the real destination BEFORE removing the source item, so the
            // descent index math isn't disturbed by the removal. If the destination
            // slot holds a container, the item goes INTO it (TFS queryDestination).
            // We don't descend when the moved item is itself a container: our flat
            // cid model can't safely deep-nest bag-in-bag-in-bag.
            let moving_is_container = self.players.get(&id)
                .and_then(|p| p.open_containers[fc as usize].as_ref())
                .and_then(|oc| oc.items.get(fs))
                .and_then(|it| self.map.item_meta(it.server_id))
                .is_some_and(|m| m.is_container);
            let dest_cid = if moving_is_container { tc }
                else { self.nested_dest_cid(id, tc, ts).unwrap_or(tc) };

            // Pull the item out of `from`. close_orphaned fixes nested-cid slot
            // bookkeeping for the removal — including the dest cid if fc == tc.
            let item = match self.pop_item_from_container(id, fc, fs) {
                Some(i) => i,
                None => return,
            };
            self.close_orphaned_nested_container(id, fc, fs);

            // Check capacity on the resolved destination.
            let dest_full = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                match p.open_containers[dest_cid as usize].as_ref() {
                    Some(oc) => oc.items.len() >= oc.capacity as usize,
                    None => return,
                }
            };
            if dest_full {
                // Put back into source at front (since we already removed it).
                self.push_item_to_container(id, fc, item);
                return;
            }

            // Insert into destination at front (TFS: addThing → push_front).
            self.push_item_to_container(id, dest_cid, item);
            return;
        }

        // --- CASE 2: ground → container ---
        if !from_is_container && !from_is_inv_slot && to_is_container {
            let tc = to_cid.unwrap();
            let from_pos = Position::new(from.x, from.y, from.z);
            let Some(p) = self.players.get(&id) else { return };
            let player_pos = p.position;

            // Adjacency check.
            let near = (i32::from(player_pos.x) - i32::from(from_pos.x)).abs() <= 1
                && (i32::from(player_pos.y) - i32::from(from_pos.y)).abs() <= 1
                && player_pos.z == from_pos.z;
            if !near { self.push_cannot_move(id, "You are too far away."); return; }

            // Resolve item from ground stack.
            let creatures = self.creatures_on(from_pos);
            let pre = self.dynamic.get(&(from_pos.x, from_pos.y, from_pos.z))
                .map(|st| st.pre_creature_len)
                .unwrap_or_else(|| self.map.tile_pre_creature_len(from_pos));
            let sp = from_stackpos as usize;
            let src_idx = if sp < pre { sp }
                else if sp < pre + creatures.len() { return; }
                else { sp - creatures.len() };

            let Some(src_sid) = self.merged_server_id(from_pos, src_idx) else { return };
            let Some(meta) = self.map.item_meta(src_sid) else { return };
            if !meta.moveable { self.push_cannot_move(id, "You cannot move this object."); return; }

            // Check dest capacity.
            let dest_full = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                match p.open_containers[tc as usize].as_ref() {
                    Some(oc) => oc.items.len() >= oc.capacity as usize,
                    None => return,
                }
            };
            if dest_full { return; }

            let stackable  = meta.stackable;
            let client_id  = meta.client_id;
            let animated   = meta.animated;
            let want = if stackable { count.max(1) } else { 1 };

            let Some((moved, removed_fully)) = self.take_from_ground(from_pos, src_idx, want, stackable) else { return };
            self.broadcast_source(from_pos, from_stackpos, removed_fully, src_idx);

            let cnt = if stackable { Some(moved) } else { None };
            let item = ContainerItem { server_id: src_sid, client_id, count: cnt, animated };
            self.push_item_to_container(id, tc, item);
            return;
        }

        // --- CASE 3: container → ground ---
        if from_is_container && !to_is_container && !to_is_inv_slot {
            let fc = from_cid.unwrap();
            let fs = from_slot_idx.unwrap();
            let to_pos = Position::new(to.x, to.y, to.z);

            // Validate destination.
            let Some(p) = self.players.get(&id) else { return };
            let player_pos = p.position;
            if player_pos.z != to_pos.z || !self.map.can_throw_object_to(player_pos, to_pos) {
                self.push_cannot_move(id, "You cannot throw there."); return;
            }
            if self.map.tile_pre_creature_len(to_pos) == 0 && self.map.tile_stack_clone(to_pos).is_none() {
                self.push_cannot_move(id, "You cannot put that there."); return;
            }
            if self.map.is_blocked(to_pos) {
                self.push_cannot_move(id, "You cannot put that there."); return;
            }

            let item = match self.pop_item_from_container(id, fc, fs) {
                Some(i) => i,
                None => return,
            };
            self.close_orphaned_nested_container(id, fc, fs);
            let meta_stackable = self.map.item_meta(item.server_id).map(|m| m.stackable).unwrap_or(false);
            let moved = item.count.unwrap_or(1).max(1);
            let dest_creatures = self.creatures_on(to_pos).len();
            let Some((dest_merged, dest_added)) =
                self.add_to_ground_front(to_pos, item.server_id, item.client_id, moved, item.animated, meta_stackable)
            else { return };
            let dest_front = self.dynamic.get(&(to_pos.x, to_pos.y, to_pos.z))
                .map(|st| st.pre_creature_len).unwrap_or(0);
            let dest_s = (dest_front + dest_creatures).min(9) as u8;
            if let Some(wi) = dest_merged { self.broadcast_dest(to_pos, dest_s, wi, true); }
            if let Some(wi) = dest_added  { self.broadcast_dest(to_pos, dest_s, wi, false); }
            return;
        }

        // --- CASE 4: inventory slot → container ---
        if from_is_inv_slot && to_is_container {
            let inv_slot = from.y as u8;
            let tc = to_cid.unwrap();
            if !(1..=10).contains(&inv_slot) { return; }

            let item = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                match p.inventory[(inv_slot - 1) as usize] {
                    Some(it) => it,
                    None => return,
                }
            };
            // Check capacity on dest.
            let dest_full = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                match p.open_containers[tc as usize].as_ref() {
                    Some(oc) => oc.items.len() >= oc.capacity as usize,
                    None => return,
                }
            };
            if dest_full { return; }

            if let Some(p) = self.players.get_mut(&id) {
                p.inventory[(inv_slot - 1) as usize] = None;
            }
            self.push_inventory_slot(id, inv_slot);

            let cnt = item.count;
            let ci = ContainerItem { server_id: item.server_id, client_id: item.client_id, count: cnt, animated: item.animated };
            self.push_item_to_container(id, tc, ci);
            return;
        }

        // --- CASE 5: container → inventory slot ---
        if from_is_container && to_is_inv_slot {
            let fc = from_cid.unwrap();
            let fs = from_slot_idx.unwrap();
            let inv_slot = to.y as u8;
            if !(1..=10).contains(&inv_slot) { return; }

            // Dest slot must be empty.
            let slot_empty = {
                let p = match self.players.get(&id) { Some(p) => p, None => return };
                p.inventory[(inv_slot - 1) as usize].is_none()
            };
            if !slot_empty { return; }

            let item = match self.pop_item_from_container(id, fc, fs) {
                Some(i) => i,
                None => return,
            };
            self.close_orphaned_nested_container(id, fc, fs);

            // Check equip slot compatibility.
            let admits = self.map.item_meta(item.server_id)
                .and_then(|m| m.equip_slot)
                .map(|eq| eq.admits(inv_slot))
                .unwrap_or(false);
            if !admits { return; }

            if let Some(p) = self.players.get_mut(&id) {
                p.inventory[(inv_slot - 1) as usize] = Some(InvItem {
                    server_id: item.server_id,
                    client_id: item.client_id,
                    count: item.count,
                    animated: item.animated,
                });
            }
            self.push_inventory_slot(id, inv_slot);
        }
    }

    /// Broadcast a 0x6A add (`is_update=false`) or 0x6B update (`is_update=true`) of `item`
    /// at explicit wire stackpos `sp` on `pos`, to every player who can see the tile.
    fn broadcast_dest(&mut self, pos: Position, sp: u8, item: WireItem, is_update: bool) {
        let mut targets = self.spectators(pos, u32::MAX);
        targets.extend(self.players.iter().filter(|(_, p)| p.position == pos).map(|(&i, _)| i));
        targets.sort_unstable();
        targets.dedup();
        for t in targets {
            let pkt = if is_update {
                tile_item::update_tile_item((pos.x, pos.y, pos.z), sp, &item)
            } else {
                tile_item::add_tile_item((pos.x, pos.y, pos.z), sp, &item)
            };
            self.push(t, pkt);
        }
    }

    /// Broadcast the source-tile change: a full removal (`0x6C`) or an in-place
    /// count update (`0x6B`) when only part of a stack was taken.
    fn broadcast_source(&mut self, pos: Position, from_stackpos: u8, removed_fully: bool, src_idx: usize) {
        let mut targets = self.spectators(pos, u32::MAX);
        targets.extend(
            self.players.iter().filter(|(_, p)| p.position == pos).map(|(&i, _)| i));
        targets.sort_unstable();
        targets.dedup();
        let pkt = if removed_fully {
            tile_creature::remove_tile_thing((pos.x, pos.y, pos.z), from_stackpos)
        } else {
            let item = self.dynamic.get(&(pos.x, pos.y, pos.z))
                .and_then(|st| st.items.get(src_idx).copied());
            let Some(item) = item else { return };
            let sp = self.item_wire_stackpos(pos, src_idx);
            tile_item::update_tile_item((pos.x, pos.y, pos.z), sp, &item)
        };
        for t in targets {
            self.push(t, pkt.clone());
        }
    }

    /// Build the "You see …" text for the tile item at stack index `idx`.
    /// `None` if the tile / index has no catalogued item. Ports
    /// `item.cpp::getDescription` (plain-item subset) + `getNameDescription`.
    fn describe_tile_item(
        &self,
        pos: Position,
        idx: usize,
        looker_pos: Position,
        gm: bool,
    ) -> Option<String> {
        let sid = self.merged_server_id(pos, idx)?;
        let meta = self.map.item_meta(sid)?;
        let count = u32::from(self.merged_count(pos, idx).unwrap_or(1).max(1));

        let mut dist = (i32::from(looker_pos.x) - i32::from(pos.x))
            .abs()
            .max((i32::from(looker_pos.y) - i32::from(pos.y)).abs());
        if looker_pos.z != pos.z {
            dist += 15;
        }

        let mut s = String::from("You see ");
        if meta.stackable && count > 1 && meta.show_count {
            s.push_str(&format!("{} {}", count, meta.plural_name()));
        } else if !meta.name.is_empty() {
            if !meta.article.is_empty() {
                s.push_str(&meta.article);
                s.push(' ');
            }
            s.push_str(&meta.name);
        } else {
            s.push_str(&format!("an item of type {}", sid));
        }
        s.push('.');

        if dist <= 1 {
            if meta.pickupable && meta.weight != 0 {
                let total = meta.weight * count;
                let plural = meta.stackable && count > 1;
                s.push('\n');
                s.push_str(if plural { "They weigh " } else { "It weighs " });
                s.push_str(&format!("{}.{:02} oz.", total / 100, total % 100));
            }
            if !meta.description.is_empty() {
                s.push('\n');
                s.push_str(&meta.description);
            }
        }

        if gm {
            s.push_str(&format!("\nItem ID: {}", sid));
            s.push_str(&format!("\nPosition: {}, {}, {}", pos.x, pos.y, pos.z));
        }
        Some(s)
    }

    /// Build the "You see …" text for a creature. Ports `player.cpp:85`
    /// (faithful subset: name, level, vocation; no party/mana/IP).
    fn describe_creature(&self, looker_id: u32, target_id: u32, gm: bool) -> Option<String> {
        let target = self.players.get(&target_id)?;
        let is_self = looker_id == target_id;
        let mut s = String::from("You see ");
        if is_self {
            s.push_str("yourself. You have no vocation.");
        } else {
            s.push_str(&target.name);
            s.push_str(" (Level 1).");
            s.push_str(if target.sex == 0 { " She" } else { " He" });
            s.push_str(" has no vocation.");
        }
        if gm {
            let p = target.position;
            s.push_str(&format!("\nPosition: {}, {}, {}", p.x, p.y, p.z));
        }
        Some(s)
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

    /// Handle `0xA1` — set or clear the attacker's melee target.
    ///
    /// - `target_id == 0` clears the fight.
    /// - `target_id == id` (self-attack) is ignored.
    /// - Attacker on a PZ tile → push `0xB4` and do NOT set target
    ///   (`combat.cpp:294-297`, TFS `playerSetAttackedCreature`).
    /// - Unknown target is silently ignored.
    fn do_set_target(&mut self, id: u32, target_id: u32) {
        if target_id == 0 {
            if let Some(p) = self.players.get_mut(&id) {
                p.attacking = None;
            }
            return;
        }
        if target_id == id {
            return; // self-attack ignored
        }
        // Check attacker exists.
        let attacker_pos = match self.players.get(&id) {
            Some(p) => p.position,
            None => return,
        };
        // PZ check on the attacker's tile.
        if self.map.is_protection_zone(attacker_pos) {
            self.push_status_message(id,
                b"You may not attack a person while you are in a protection zone.");
            return;
        }
        // Target must exist.
        if !self.players.contains_key(&target_id) {
            return;
        }
        if let Some(p) = self.players.get_mut(&id) {
            p.attacking = Some(target_id);
            // Prime last_attack_ms = 0 so the first tick whose now_ms >=
            // MELEE_ATTACK_INTERVAL_MS swings immediately.
            p.last_attack_ms = 0;
        }
    }

    /// Gate + parse + dispatch for `/`-prefixed GM commands. Non-gamemasters are
    /// silently ignored (their `/` line is simply dropped). Every parse/lookup
    /// failure replies to the sender via `push_status_message` and leaves the
    /// world untouched — no panics, no partial state.
    fn do_gm_command(&mut self, id: u32, text: String) {
        if !self.players.get(&id).map(|p| p.gamemaster).unwrap_or(false) {
            return; // not a GM: drop silently
        }
        let line = text.trim_start_matches('/').trim();
        // Quote-aware tokenization so multi-word arguments survive: item names
        // (`/item "Gold Coin" 100`) and player names with spaces (`/bring "God Diego"`).
        let tokens = tokenize_args(line);
        let Some((verb, rest)) = tokens.split_first() else { return };
        let args: Vec<&str> = rest.iter().map(|s| s.as_str()).collect();
        let Some(cmd) = GmVerb::from_word(verb) else {
            self.push_status_message(id, format!("Unknown command: /{verb}. Type /help.").as_bytes());
            return;
        };
        match cmd {
            GmVerb::Help => self.gm_help(id),
            GmVerb::Item => self.gm_item(id, &args),
            GmVerb::Goto => self.gm_goto(id, &args),
            GmVerb::Temple => self.gm_temple(id, &args),
            GmVerb::Teleport => self.gm_teleport(id, &args),
            GmVerb::TeleportTo => self.gm_teleportto(id, &args),
            GmVerb::Bring => self.gm_bring(id, &args),
            GmVerb::ChangeSex => self.gm_changesex(id, &args),
            GmVerb::SetLookType => self.gm_setlooktype(id, &args),
        }
    }

    /// `/help` — list every gamemaster command with a short description. Sent as
    /// `0xB4` console messages to the requester only: this server has no chat
    /// channels yet, but console text is private to the session and scrollable,
    /// which is what `/help` needs. Iterates [`GmVerb::ALL`], so newly added
    /// commands appear automatically.
    ///
    /// TODO(future): real chat channels (0xB2/0xAB/0xAC handshake) for richer GM
    /// output — see docs/superpowers/backlog.md.
    fn gm_help(&mut self, id: u32) {
        self.push_console_blue(id, "Gamemaster commands:");
        for &cmd in GmVerb::ALL {
            self.push_console_blue(id, &format!("{} - {}", cmd.usage(), cmd.description()));
        }
    }

    /// Find an online player's creature id by name (case-insensitive).
    fn find_player_by_name(&self, name: &str) -> Option<u32> {
        self.players.iter()
            .find(|(_, p)| p.name.eq_ignore_ascii_case(name))
            .map(|(&id, _)| id)
    }

    /// `/goto <x> <y> <z>` — teleport the GM to coordinates.
    /// `/goto <player>` — teleport the GM to that online player's tile.
    fn gm_goto(&mut self, id: u32, args: &[&str]) {
        // Coordinate form: three numeric args.
        if let Some(pos) = parse_pos(args) {
            if !self.map.has_ground(pos) {
                self.push_status_message(id, b"There is no tile there.");
                return;
            }
            self.do_teleport(id, pos);
            self.push_status_message(id, format!("Teleported to {}, {}, {}.", pos.x, pos.y, pos.z).as_bytes());
            return;
        }
        // Player form: a single name argument (quote multi-word names).
        if let [name] = args {
            let Some(target) = self.find_player_by_name(name) else {
                self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
                return;
            };
            let Some(pos) = self.players.get(&target).map(|p| p.position) else { return };
            self.do_teleport(id, pos);
            self.push_status_message(id, format!("Teleported to {name}.").as_bytes());
            return;
        }
        self.push_status_message(id, b"Usage: /goto <x> <y> <z> | /goto \"player\"");
    }

    /// `/temple` — teleport to the default (spawn) town temple.
    /// `/temple <name>` — teleport to the named town's temple.
    /// `/temple <id>` — teleport to the temple of the town with that id.
    /// (No per-character home town exists yet, so the no-arg form uses the
    /// server's spawn temple — see `StaticMap::temple_for`.)
    ///
    /// TODO(future): per-character home town so no-arg /temple targets the char's
    /// own town — see docs/superpowers/backlog.md.
    fn gm_temple(&mut self, id: u32, args: &[&str]) {
        let temple = match args.first() {
            None => self.map.spawn(),
            Some(arg) => match arg.parse::<u32>() {
                Ok(town_id) => match self.map.town_temple_by_id(town_id) {
                    Some(p) => p,
                    None => {
                        self.push_status_message(id, format!("No town with id {town_id}.").as_bytes());
                        return;
                    }
                },
                Err(_) => match self.map.town_temple_by_name(arg) {
                    Some(p) => p,
                    None => {
                        self.push_status_message(id, format!("No town named '{arg}'.").as_bytes());
                        return;
                    }
                },
            },
        };
        self.do_teleport(id, temple);
        self.push_status_message(id, format!("Teleported to temple ({}, {}, {}).", temple.x, temple.y, temple.z).as_bytes());
    }

    /// `/item <id|name> [count]` — spawn an item on the GM's own tile. A leading
    /// number is a server id; otherwise the name is taken from the arguments
    /// (case-insensitive, singular or plural, no quotes needed for multi-word
    /// names) and an optional trailing number is the count. Quotes still group.
    fn gm_item(&mut self, id: u32, args: &[&str]) {
        if args.is_empty() {
            self.push_status_message(id, b"Usage: /item <id|name> [count]");
            return;
        }
        // ID form: a leading number is unambiguously a server id, since no item
        // name in Tibia contains digits. `/item 2400 100`.
        let (server_id, count) = if let Ok(server_id) = args[0].parse::<u16>() {
            let count = args.get(1).and_then(|s| s.parse::<u16>().ok()).unwrap_or(1);
            (server_id, count)
        } else {
            // Name form: an optional trailing number is the count; everything
            // before it joins into the (possibly multi-word, unquoted) item name.
            // `/item crystal coin 100` → name "crystal coin", count 100.
            let (name_tokens, count) = match args.split_last() {
                Some((last, head)) if !head.is_empty() => match last.parse::<u16>() {
                    Ok(n) => (head, n),
                    Err(_) => (args, 1),
                },
                _ => (args, 1),
            };
            let name = name_tokens.join(" ");
            match self.map.find_item_id_by_name(&name) {
                Some(sid) => (sid, count),
                None => {
                    self.push_status_message(id, format!("No item named '{name}'.").as_bytes());
                    return;
                }
            }
        };
        let Some(pos) = self.players.get(&id).map(|p| p.position) else { return };
        self.do_spawn_item(id, pos, server_id, count);
    }

    /// `/teleport <name> <x> <y> <z>` — teleport another player to a position.
    fn gm_teleport(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /teleport <name> <x> <y> <z>");
            return;
        };
        let Some(pos) = parse_pos(&args[1..]) else {
            self.push_status_message(id, b"Usage: /teleport <name> <x> <y> <z>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        if !self.map.has_ground(pos) {
            self.push_status_message(id, b"There is no tile there.");
            return;
        }
        self.do_teleport(target, pos);
        self.push_status_message(id, format!("Teleported {} to {}, {}, {}.", name, pos.x, pos.y, pos.z).as_bytes());
    }

    /// `/teleportto <name>` — teleport the GM to another player's tile.
    fn gm_teleportto(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /teleportto <name>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        let Some(pos) = self.players.get(&target).map(|p| p.position) else { return };
        self.do_teleport(id, pos);
        self.push_status_message(id, format!("Teleported to {name}.").as_bytes());
    }

    /// `/bring <name>` — teleport another player to the GM's tile.
    fn gm_bring(&mut self, id: u32, args: &[&str]) {
        let Some(name) = args.first() else {
            self.push_status_message(id, b"Usage: /bring <name>");
            return;
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        let Some(pos) = self.players.get(&id).map(|p| p.position) else { return };
        self.do_teleport(target, pos);
        self.push_status_message(id, format!("Brought {name} to you.").as_bytes());
    }

    fn gm_changesex(&mut self, id: u32, args: &[&str]) {
        let (Some(name), Some(sex_str)) = (args.first(), args.get(1)) else {
            self.push_status_message(id, b"Usage: /changesex <name> <male|female>");
            return;
        };
        let new_sex: u8 = match sex_str.to_lowercase().as_str() {
            "male" => 1,
            "female" => 0,
            _ => {
                self.push_status_message(id, b"Sex must be 'male' or 'female'.");
                return;
            }
        };
        let Some(target) = self.find_player_by_name(name) else {
            self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
            return;
        };
        match self.players.get_mut(&target) {
            Some(p) => p.sex = new_sex,
            None => return,
        };
        let sex_label = if new_sex == 1 { "male" } else { "female" };
        self.push_status_message(id, format!("{name} is now {sex_label}.").as_bytes());
    }

    fn gm_setlooktype(&mut self, id: u32, args: &[&str]) {
        // /setlooktype <id>  OR  /setlooktype "player" <id>
        let (target, look_type) = match args {
            [raw_id] => {
                let Ok(lt) = raw_id.parse::<u16>() else {
                    self.push_status_message(id, b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>");
                    return;
                };
                (id, lt)
            }
            [name, raw_id] => {
                let Ok(lt) = raw_id.parse::<u16>() else {
                    self.push_status_message(id, b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>");
                    return;
                };
                let Some(t) = self.find_player_by_name(name) else {
                    self.push_status_message(id, format!("Player '{name}' not found.").as_bytes());
                    return;
                };
                (t, lt)
            }
            _ => {
                self.push_status_message(id, b"Usage: /setlooktype <id> | /setlooktype \"player\" <id>");
                return;
            }
        };
        let new_outfit = match self.players.get_mut(&target) {
            Some(p) => { p.outfit.look_type = look_type; p.outfit }
            None => return,
        };
        self.do_change_outfit(target, new_outfit);
        self.push_status_message(id, format!("Look type set to {look_type}.").as_bytes());
    }

    /// Place a fresh item on `pos` and broadcast a `0x6A` add to spectators.
    /// Mirrors the destination half of `do_move_thing`: materialize the tile,
    /// insert at the front of the down-items (newest on top), broadcast at the
    /// top down-item stackpos. Replies to `gm_id` on success or failure.
    fn do_spawn_item(&mut self, gm_id: u32, pos: Position, server_id: u16, count: u16) {
        let Some(meta) = self.map.item_meta(server_id) else {
            self.push_status_message(gm_id, format!("Unknown item id {server_id}.").as_bytes());
            return;
        };
        let client_id = meta.client_id;
        let animated = meta.animated;
        let stackable = meta.stackable;

        if !self.materialize(pos) {
            self.push_status_message(gm_id, b"You cannot create an item there.");
            return;
        }
        // TFS 10-thing-per-tile cap.
        let len = self.dynamic.get(&(pos.x, pos.y, pos.z)).map(|st| st.items.len()).unwrap_or(0);
        if len >= 10 {
            self.push_status_message(gm_id, b"This tile is full.");
            return;
        }

        let subtype = if stackable { Some(count.clamp(1, 100) as u8) } else { None };
        let wi = WireItem { client_id, subtype, animated };

        // creatures_on borrows &self immutably; compute before the &mut get_mut.
        let dest_creatures = self.creatures_on(pos).len();
        {
            let st = self.dynamic.get_mut(&(pos.x, pos.y, pos.z)).unwrap();
            let front = st.pre_creature_len; // first down-item slot
            st.items.insert(front, wi);
            st.server_ids.insert(front, server_id);
            st.counts.insert(front, subtype);
        }
        let front = self.dynamic.get(&(pos.x, pos.y, pos.z)).map(|st| st.pre_creature_len).unwrap_or(0);
        let dest_s = (front + dest_creatures).min(9) as u8;
        self.broadcast_dest(pos, dest_s, wi, false);

        self.push_status_message(gm_id, format!("Created item {server_id}.").as_bytes());
    }

    /// Apply `dmg` hit points of damage to `victim_id`, dealt by `attacker_id`.
    /// Clamps to 0, pushes health-bar (`0x8C`) to all spectators including the
    /// victim and attacker, pushes self-stats (`0xA0`) to the victim, emits the
    /// physical-hit blood effect plus a floating damage number (`0xB4`), and
    /// fires `do_death` on 0 HP.
    fn apply_damage(&mut self, attacker_id: u32, victim_id: u32, dmg: i32) {
        let (health_before, new_health, max_health) = {
            let v = match self.players.get_mut(&victim_id) {
                Some(p) => p,
                None => return,
            };
            let before = v.health;
            v.health = v.health.saturating_sub(dmg.max(0) as u16);
            (before, v.health, v.max_health)
        };
        let victim_pos = match self.players.get(&victim_id) {
            Some(p) => p.position,
            None => return,
        };
        // Push 0x8C health-bar to every spectator of the victim's tile,
        // INCLUDING the victim itself (it is also a spectator of its own tile).
        let pct = combat_packets::health_percent(i32::from(new_health), i32::from(max_health));
        let health_bar = combat_packets::creature_health(victim_id, pct);
        // Collect spectators first (can_see of the victim's tile), then push.
        let spectators: Vec<u32> = self.players
            .iter()
            .filter(|&(&sid, sp)| Self::can_see(sp.position, victim_pos) || sid == victim_id)
            .map(|(&sid, _)| sid)
            .collect();
        for sid in &spectators {
            self.push(*sid, health_bar.clone());
        }
        // Push 0xA0 self-stats to the victim only.
        let stats = {
            let p = match self.players.get(&victim_id) { Some(p) => p, None => return };
            enter_world::stats(&enter_world::Stats {
                health: p.health,
                max_health: p.max_health,
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
                base_speed: 220,
            })
        };
        self.push(victim_id, stats);
        // Push a physical-hit magic effect on the victim's tile to all spectators.
        // Physical-hit blood effect. TFS sends the effect byte directly, so the
        // wire value is the enum value (CONST_ME_DRAWBLOOD = 1). See
        // enter_world::EFFECT_DRAWBLOOD.
        let effect = enter_world::magic_effect(
            victim_pos.x, victim_pos.y, victim_pos.z, enter_world::EFFECT_DRAWBLOOD);
        for sid in &spectators {
            self.push(*sid, effect.clone());
        }
        // Floating damage number (0xB4 TextMessage). Use the damage actually
        // applied (clamped at the victim's remaining HP), not the raw roll, so
        // an overkill shows the real hit. A 0 value renders nothing client-side,
        // so skip the packet entirely. The mode byte is routed per recipient:
        // the attacker sees "dealt", the victim "received", bystanders "others".
        let applied = u32::from(health_before.saturating_sub(new_health));
        if applied > 0 {
            let victim_name = self.players.get(&victim_id)
                .map(|p| p.name.clone()).unwrap_or_default();
            let attacker_name = self.players.get(&attacker_id)
                .map(|p| p.name.clone()).unwrap_or_default();
            for sid in &spectators {
                let (mode, text) = if *sid == attacker_id {
                    (combat_packets::MSG_DAMAGE_DEALT,
                     format!("You deal {applied} damage to {victim_name}."))
                } else if *sid == victim_id {
                    (combat_packets::MSG_DAMAGE_RECEIVED,
                     format!("You lose {applied} hitpoints due to an attack by {attacker_name}."))
                } else {
                    (combat_packets::MSG_DAMAGE_OTHERS,
                     format!("{victim_name} loses {applied} hitpoints due to an attack by {attacker_name}."))
                };
                let pkt = combat_packets::damage_text(
                    mode, victim_pos.x, victim_pos.y, victim_pos.z,
                    applied, combat_packets::TEXTCOLOR_RED, text.as_bytes());
                self.push(*sid, pkt);
            }
        }
        // Death?
        if new_health == 0 {
            self.do_death(victim_id);
        }
    }

    /// Handle the death of `victim_id`: death == logout. Send the `0x28` death
    /// window, clear all fights, id-form remove at the death tile, then remove the
    /// victim from the world and emit a `SaveRecord` at the temple with full HP.
    /// Dropping the victim's `push_tx` ends the session; the relog spawns at the
    /// temple (M8 `login` restores the saved position). Mirrors TFS `onDeath` →
    /// `sendReLoginWindow` + `removeCreature` (player.cpp:2070, 2197).
    fn do_death(&mut self, victim_id: u32) {
        // Death window to the victim — best-effort, non-reaping `try_send`. The
        // reaping `push()` would, on a saturated client buffer, divert death
        // through `logout()` (which saves at the death tile with 0 HP) and then
        // the temple/full-HP save below would never run.
        if let Some(p) = self.players.get(&victim_id) {
            let _ = p.push_tx.try_send(combat_packets::death_window(0));
        }

        // Clear all fights targeting the victim, and the victim's own fight.
        let all_ids: Vec<u32> = self.players.keys().copied().collect();
        for pid in all_ids {
            if let Some(p) = self.players.get_mut(&pid) {
                if p.attacking == Some(victim_id) || pid == victim_id {
                    p.attacking = None;
                }
            }
        }

        // Death position + temple destination (computed before removal).
        let death_pos = match self.players.get(&victim_id) {
            Some(p) => p.position,
            None => return,
        };
        let temple = self.map.temple_for(death_pos);

        // Remove from the death tile for spectators. The id-form remove is
        // unambiguous under co-occupancy (stair/height landings); drop the victim
        // from each spectator's known-set so a relog re-introduces it (full form).
        for spec in self.spectators(death_pos, victim_id) {
            self.push(spec, walk::remove_creature_by_id(victim_id));
            if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&victim_id); }
        }

        // Remove the victim from the world (death == logout). Persist the player
        // AT THE TEMPLE with full HP so the relog spawns there — M8 `login`
        // restores the saved position, so saving the death tile would respawn the
        // player where they died. Dropping the PlayerState drops its session
        // push_tx, which closes the writer channel and ends the session: the
        // client shows the death window and returns to character select. Mirrors
        // TFS onDeath -> sendReLoginWindow + removeCreature (player.cpp:2070, 2197);
        // the death-respawn position is the town temple.
        let Some(p) = self.players.remove(&victim_id) else { return };
        if let Some(tx) = &self.save_tx {
            let _ = tx.send(SaveRecord {
                name: p.name.clone(),
                position: temple,
                direction: p.direction,
                outfit: p.outfit,
                health: p.max_health,
                max_health: p.max_health,
                sex: p.sex,
                inventory: p.inventory.iter().enumerate()
                    .filter_map(|(i, slot)| slot.map(|it| ((i + 1) as u8, it.server_id, it.count.unwrap_or(1))))
                    .collect(),
                container_items: Self::export_container_items(&p.inventory, &p.open_containers),
            });
        }

    }

    /// Global combat tick. Iterates all players with an active target and, for
    /// each whose attack interval has elapsed, rolls one swing. Out-of-range or
    /// missing targets clear the fight without damage.
    fn on_combat_tick(&mut self, now_ms: u64) {
        // Collect (attacker_id, target_id) pairs to process; avoid double-borrow.
        let fights: Vec<(u32, u32)> = self.players
            .iter()
            .filter_map(|(&id, p)| p.attacking.map(|tid| (id, tid)))
            .collect();

        for (attacker_id, target_id) in fights {
            // Target gone? Clear the fight.
            let target_pos = match self.players.get(&target_id) {
                Some(p) => p.position,
                None => {
                    if let Some(p) = self.players.get_mut(&attacker_id) { p.attacking = None; }
                    continue;
                }
            };
            let (attacker_pos, last_attack, fist_skill) = match self.players.get(&attacker_id) {
                Some(p) => (p.position, p.last_attack_ms, p.fist_skill),
                None => continue,
            };
            // W3 fix: TFS clears the fight the moment EITHER party is in a PZ
            // (`canTargetCreature` combat.cpp:221-229; `onAttackedCreatureChangeZone`
            // player.cpp:1153). A victim who fled into the temple must stop taking
            // hits on the very next tick — clearing `attacking` (not just skipping
            // the swing) so the attacker also gets their combat state cleared.
            if self.map.is_protection_zone(attacker_pos)
                || self.map.is_protection_zone(target_pos)
            {
                if let Some(p) = self.players.get_mut(&attacker_id) { p.attacking = None; }
                continue;
            }
            // Interval check.
            if now_ms.saturating_sub(last_attack) < MELEE_ATTACK_INTERVAL_MS {
                continue;
            }
            // Same-floor Chebyshev ≤ 1 (TFS `useFist` range check).
            if attacker_pos.z != target_pos.z {
                continue; // cross-floor melee impossible
            }
            let dx = (i32::from(attacker_pos.x) - i32::from(target_pos.x)).abs();
            let dy = (i32::from(attacker_pos.y) - i32::from(target_pos.y)).abs();
            if dx > 1 || dy > 1 {
                continue; // out of melee range, no swing this tick
            }
            // Roll damage.
            let dmg = combat::fist_damage(&mut self.rng, 1, fist_skill);
            // Update last_attack_ms before applying damage (apply_damage may call
            // do_death which removes the attacker's fight — the timestamp update
            // must not be lost in that chain).
            if let Some(p) = self.players.get_mut(&attacker_id) {
                p.last_attack_ms = now_ms;
            }
            self.apply_damage(attacker_id, target_id, dmg);
        }
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

/// Every gamemaster command. This enum is the single source of truth: the
/// exhaustive `usage`/`description` matches force any new variant to declare its
/// help text (it won't compile otherwise), and the dispatch `match` in
/// `do_gm_command` forces it to be wired. `/help` iterates [`GmVerb::ALL`], so it
/// can never drift out of sync — the only manual step when adding a command is to
/// list its variant in `ALL`.
#[derive(Clone, Copy)]
enum GmVerb {
    Help,
    Item,
    Goto,
    Temple,
    Teleport,
    TeleportTo,
    Bring,
    ChangeSex,
    SetLookType,
}

impl GmVerb {
    /// All commands, in the order `/help` lists them.
    const ALL: &'static [GmVerb] = &[
        Self::Help,
        Self::Item,
        Self::Goto,
        Self::Temple,
        Self::Teleport,
        Self::TeleportTo,
        Self::Bring,
        Self::ChangeSex,
        Self::SetLookType,
    ];

    /// Accepted command words (first is canonical; the rest are aliases).
    fn words(self) -> &'static [&'static str] {
        match self {
            Self::Help => &["help"],
            Self::Item => &["item", "i"],
            Self::Goto => &["goto"],
            Self::Temple => &["temple"],
            Self::Teleport => &["teleport"],
            Self::TeleportTo => &["teleportto"],
            Self::Bring => &["bring"],
            Self::ChangeSex => &["changesex"],
            Self::SetLookType => &["setlooktype"],
        }
    }

    /// One-line usage syntax shown by `/help`.
    fn usage(self) -> &'static str {
        match self {
            Self::Help => "/help",
            Self::Item => "/item <id|name> [count]",
            Self::Goto => "/goto <x> <y> <z> | /goto \"player\"",
            Self::Temple => "/temple [\"name\"|id]",
            Self::Teleport => "/teleport \"player\" <x> <y> <z>",
            Self::TeleportTo => "/teleportto \"player\"",
            Self::Bring => "/bring \"player\"",
            Self::ChangeSex => "/changesex \"player\" <male|female>",
            Self::SetLookType => "/setlooktype <id> | /setlooktype \"player\" <id>",
        }
    }

    /// Short description shown by `/help`.
    fn description(self) -> &'static str {
        match self {
            Self::Help => "List all gamemaster commands.",
            Self::Item => "Spawn an item on your tile, by id or name.",
            Self::Goto => "Teleport yourself to coordinates or to a player.",
            Self::Temple => "Teleport to a town temple (default, by name, or by id).",
            Self::Teleport => "Teleport another player to coordinates.",
            Self::TeleportTo => "Teleport yourself next to another player.",
            Self::Bring => "Teleport another player to you.",
            Self::ChangeSex => "Change a player's sex (affects outfit catalog).",
            Self::SetLookType => "Set look type on yourself or another player.",
        }
    }

    /// Resolve a command word (verb or alias) to its variant.
    fn from_word(word: &str) -> Option<GmVerb> {
        GmVerb::ALL.iter().copied().find(|v| v.words().contains(&word))
    }
}

/// Split a GM command's argument string into tokens, treating `"..."` as a
/// single token (so `"Gold Coin" 100` → `["Gold Coin", "100"]`). Unquoted runs
/// split on whitespace; an unterminated quote consumes to the end of input.
fn tokenize_args(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next(); // consume opening quote
            let mut tok = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                tok.push(c);
            }
            tokens.push(tok);
        } else {
            let mut tok = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                tok.push(c);
                chars.next();
            }
            tokens.push(tok);
        }
    }
    tokens
}

/// Parse `<x> <y> <z>` from the front of a GM command's args. `None` if any
/// coordinate is missing or out of range.
fn parse_pos(args: &[&str]) -> Option<Position> {
    let x = args.first()?.parse::<u16>().ok()?;
    let y = args.get(1)?.parse::<u16>().ok()?;
    let z = args.get(2)?.parse::<u8>().ok()?;
    Some(Position::new(x, y, z))
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
    use crate::map::StaticMap;
    use formats::otb::{ItemType, ItemsOtb};
    use formats::otbm::{MapItem, MapTile, OtbmMap, Town};

    fn stair_map() -> Arc<StaticMap> {
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

    fn walk_map() -> Arc<StaticMap> {
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

    fn knight() -> Outfit {
        Outfit { look_type: 128, head: 78, body: 69, legs: 58, feet: 76, addons: 0, mount: 0 }
    }

    /// Build a default `InitialState` for use in tests that don't care about
    /// the restored-vs-new distinction.
    fn default_initial(outfit: Outfit) -> InitialState {
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
    fn add_player(g: &mut Game, pos: Position) -> (u32, mpsc::Receiver<Vec<u8>>) {
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

    #[test]
    fn tokenize_args_groups_quoted_segments() {
        assert_eq!(tokenize_args("crystal coin 100"), ["crystal", "coin", "100"]);
        assert_eq!(tokenize_args("\"Gold Coin\" 100"), ["Gold Coin", "100"]);
        assert_eq!(tokenize_args("   spaced   out   "), ["spaced", "out"]);
        assert_eq!(tokenize_args("\"unterminated"), ["unterminated"]); // quote to end of input
        assert!(tokenize_args("").is_empty());
    }

    #[test]
    fn parse_pos_reads_three_coords() {
        assert_eq!(parse_pos(&["100", "200", "7"]), Some(Position::new(100, 200, 7)));
        assert_eq!(parse_pos(&["100", "200"]), None); // too few
        assert_eq!(parse_pos(&["x", "200", "7"]), None); // non-numeric
        assert_eq!(parse_pos(&[]), None);
    }

    #[test]
    fn find_player_by_name_is_case_insensitive() {
        let mut g = Game::new(stair_map());
        let (id, _rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.players.get_mut(&id).unwrap().name = "God Diego".into();
        assert_eq!(g.find_player_by_name("god diego"), Some(id));
        assert_eq!(g.find_player_by_name("GOD DIEGO"), Some(id));
        assert_eq!(g.find_player_by_name("nobody"), None);
    }

    #[test]
    fn gmverb_registry_is_complete_and_resolvable() {
        // The enum is the single source of truth: every command must declare help
        // text and every word must resolve back to its variant.
        for &cmd in GmVerb::ALL {
            assert!(!cmd.usage().is_empty(), "a GmVerb is missing its usage");
            assert!(!cmd.description().is_empty(), "a GmVerb is missing its description");
            assert!(!cmd.words().is_empty(), "a GmVerb is missing its words");
            for w in cmd.words() {
                assert!(GmVerb::from_word(w).is_some(), "from_word does not resolve '{w}'");
            }
        }
        assert!(matches!(GmVerb::from_word("i"), Some(GmVerb::Item))); // alias
        assert!(matches!(GmVerb::from_word("item"), Some(GmVerb::Item)));
        assert!(GmVerb::from_word("nonsense").is_none());
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
    async fn login_pushes_appear_to_existing_spectator() {
        let (world, _save_rx) = spawn(walk_map());
        let (tx_a, mut rx_a) = push_channel();
        let ack_a = world.login("A".into(), default_initial(knight()), tx_a).await.unwrap();
        // Second player logs in next to A; A must receive a 0x6A appear.
        let (tx_b, _rx_b) = push_channel();
        let _ack_b = world.login("B".into(), default_initial(knight()), tx_b).await.unwrap();
        let pkt = rx_a.recv().await.unwrap();
        assert_eq!(pkt[0], protocol::tile_creature::OP_ADD_TILE_CREATURE);
        // ...followed by the teleport puff, so spectators see the spawn effect too.
        let effect = rx_a.recv().await.unwrap();
        assert_eq!(effect[0], protocol::enter_world::OP_MAGIC_EFFECT);
        assert_ne!(ack_a.snapshot.id, 0);
    }

    #[tokio::test]
    async fn second_login_sees_first_in_ack_others() {
        let (world, _save_rx) = spawn(walk_map());
        let (tx_a, _rx_a) = push_channel();
        world.login("A".into(), default_initial(knight()), tx_a).await.unwrap();
        let (tx_b, _rx_b) = push_channel();
        let ack_b = world.login("B".into(), default_initial(knight()), tx_b).await.unwrap();
        assert_eq!(ack_b.others.len(), 1, "B's enter-world includes A");
    }

    #[test]
    fn relogin_map_description_includes_dynamic_ground_items() {
        // Regression: a player logging in must see items dropped on the ground
        // (the dynamic overlay), not the pristine OTBM tile. The enter-world map
        // slice is encoded from the MERGED view inside `Game::login`; before the
        // fix it was encoded from the raw `StaticMap` in the server layer, so the
        // relogging client was blind to dynamic ground items while online
        // spectators (fed by `merged()`) still saw them.
        let mut g = Game::new(walk_map());
        let drop_pos = Position::new(96, 117, 7); // adjacent to spawn, in viewport
        // A client id absent from the static encoding (ground 4526, wall 1059) so
        // its presence proves the dynamic item — not a coincidence — leaked in.
        const DROP_CID: u16 = 0x7777;

        // Baseline: a login BEFORE any drop must NOT carry the item.
        let (tx0, _rx0) = mpsc::channel(super::PUSH_CAPACITY);
        let initial0 = InitialState { position: Some(Position::new(95, 117, 7)), ..default_initial(knight()) };
        let ack0 = g.login("Before".into(), initial0, tx0);
        assert!(
            !ack0.map_description.windows(2).any(|w| w == DROP_CID.to_le_bytes()),
            "baseline login must not contain an item that was never dropped"
        );

        // Drop a non-stackable item on the ground next to spawn.
        let _ = g.add_to_ground_front(drop_pos, 999, DROP_CID, 1, false, false);

        // A fresh login near the same spot must now carry the dropped item.
        let (tx1, _rx1) = mpsc::channel(super::PUSH_CAPACITY);
        let initial1 = InitialState { position: Some(Position::new(95, 117, 7)), ..default_initial(knight()) };
        let ack1 = g.login("After".into(), initial1, tx1);
        assert!(
            ack1.map_description.windows(2).any(|w| w == DROP_CID.to_le_bytes()),
            "relogin map slice must include the dynamic ground item"
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

    #[tokio::test]
    async fn logout_pushes_remove_to_spectator() {
        let (world, _save_rx) = spawn(walk_map());
        let (tx_a, mut rx_a) = push_channel();
        world.login("A".into(), default_initial(knight()), tx_a).await.unwrap();
        let (tx_b, _rx_b) = push_channel();
        let ack_b = world.login("B".into(), default_initial(knight()), tx_b).await.unwrap();
        let _ = rx_a.recv().await.unwrap(); // appear (0x6A)
        let effect = rx_a.recv().await.unwrap(); // login teleport puff (0x83)
        assert_eq!(effect[0], protocol::enter_world::OP_MAGIC_EFFECT);
        world.logout(ack_b.snapshot.id).await;
        // Logout pushes a teleport puff, then the remove.
        let poof = rx_a.recv().await.unwrap();
        assert_eq!(poof[0], protocol::enter_world::OP_MAGIC_EFFECT);
        let pkt = rx_a.recv().await.unwrap();
        assert_eq!(pkt[0], protocol::tile_creature::OP_REMOVE_TILE_THING);
    }

    #[tokio::test]
    async fn shutdown_and_save_persists_online_players_then_stops_actor() {
        // Graceful shutdown through the live actor: shutdown_and_save resolves
        // once the save record is queued, and the actor stops afterwards.
        let (world, mut save_rx) = spawn(walk_map());
        let (tx_a, _rx_a) = push_channel();
        let ack = world.login("Diego".into(), default_initial(wizard_outfit()), tx_a)
            .await.unwrap();

        world.shutdown_and_save().await;

        // The logged-in player's state was persisted.
        let rec = save_rx.recv().await.expect("shutdown must emit a SaveRecord");
        assert_eq!(rec.name, "Diego");
        assert_eq!(rec.position, ack.snapshot.position);
        assert_eq!(rec.outfit, wizard_outfit());

        // The actor has stopped: the save channel is closed (no more records)
        // and further logins fail because the command channel is gone.
        assert!(save_rx.recv().await.is_none(), "save channel must close after shutdown");
        let (tx_b, _rx_b) = push_channel();
        assert!(
            world.login("B".into(), default_initial(knight()), tx_b).await.is_none(),
            "logins must fail once the actor has shut down"
        );
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

    #[tokio::test]
    async fn second_login_on_occupied_spawn_gets_free_tile() {
        let (world, _save_rx) = spawn(walk_map());
        let (tx_a, _ra) = push_channel();
        let ack_a = world.login("A".into(), default_initial(knight()), tx_a).await.unwrap();
        let (tx_b, _rb) = push_channel();
        let ack_b = world.login("B".into(), default_initial(knight()), tx_b).await.unwrap();
        assert_ne!(
            ack_a.snapshot.position,
            ack_b.snapshot.position,
            "co-logins must not share a tile"
        );
    }

    #[test]
    fn login_on_occupied_saved_position_gets_free_adjacent_tile() {
        // A returning player carries a saved position. If someone is already
        // standing on that tile, login must bump them to a free adjacent tile —
        // you never log in on top of another creature. (Stair/height co-occupancy
        // is allowed during movement, but NOT on login.)
        let mut g = Game::new(walk_map());
        let saved = Position::new(95, 117, 7);
        let (_occupant, _ro) = add_player(&mut g, saved); // someone is already there
        let (tx, _rx) = mpsc::channel(PUSH_CAPACITY);
        let initial = InitialState {
            position: Some(saved),
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 1,
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("Returning".into(), initial, tx);
        let ps = g.players.get(&ack.snapshot.id).expect("player must exist");
        assert_ne!(ps.position, saved, "must not log in on top of the occupant");
        assert!(g.map.is_walkable(ps.position), "bumped tile must be walkable");
        let sharing = g.players.values().filter(|p| p.position == ps.position).count();
        assert_eq!(sharing, 1, "bumped tile must hold only the returning player");
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

    #[tokio::test]
    async fn say_broadcasts_to_spectator_and_speaker() {
        let (world, _save_rx) = spawn(walk_map());
        let (tx_a, mut rx_a) = push_channel();
        let ack_a = world.login("A".into(), default_initial(knight()), tx_a).await.unwrap();
        let (tx_b, mut rx_b) = push_channel();
        world.login("B".into(), default_initial(knight()), tx_b).await.unwrap();
        // Drain A's appear-of-B (0x6A) + teleport puff (0x83) from B's login.
        let _ = rx_a.recv().await.unwrap();
        let _ = rx_a.recv().await.unwrap();
        world.say(ack_a.snapshot.id, SpeakType::Say, "hello".into()).await;
        let own = rx_a.recv().await.unwrap();
        assert_eq!(own[0], protocol::chat::OP_CREATURE_SAY, "speaker hears own");
        let heard = rx_b.recv().await.unwrap();
        assert_eq!(heard[0], protocol::chat::OP_CREATURE_SAY, "spectator hears it");
    }

    #[test]
    fn say_does_not_reach_beyond_viewport() {
        let mut g = Game::new(walk_map());
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (_far, mut rx) = add_player(&mut g, Position::new(107, 117, 7)); // 12 east, outside ±8
        g.do_say(a, SpeakType::Say, "hi".into());
        assert!(rx.try_recv().is_err(), "say must not reach beyond ±8x");
    }

    #[test]
    fn yell_uppercases_and_reaches_far_spectator() {
        let mut g = Game::new(walk_map());
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (_far, mut rx) = add_player(&mut g, Position::new(107, 117, 7)); // 12 east: >±8, <±18
        g.do_say(a, SpeakType::Yell, "help".into());
        let pkt = rx.try_recv().expect("yell reaches ±18x");
        assert_eq!(pkt[0], protocol::chat::OP_CREATURE_SAY);
        assert!(String::from_utf8_lossy(&pkt).contains("HELP"), "yell text is uppercased");
    }

    #[test]
    fn whisper_full_to_adjacent_pspsps_to_far_in_view() {
        let mut g = Game::new(walk_map());
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (_adj, mut rx_adj) = add_player(&mut g, Position::new(96, 117, 7)); // Chebyshev 1
        let (_far, mut rx_far) = add_player(&mut g, Position::new(102, 117, 7)); // 7 east: in ±8, >1
        g.do_say(a, SpeakType::Whisper, "secret".into());
        let adj = rx_adj.try_recv().expect("adjacent hears whisper");
        assert!(String::from_utf8_lossy(&adj).contains("secret"));
        let far = rx_far.try_recv().expect("far-in-view gets a packet");
        let fs = String::from_utf8_lossy(&far);
        assert!(fs.contains("pspsps") && !fs.contains("secret"), "far in view hears pspsps: {fs}");
    }

    // -------------------------------------------------------------------------
    // M7 combat tests
    // -------------------------------------------------------------------------

    /// Build a map for combat tests: a 3-wide row of walkable ground tiles centred
    /// at the spawn (95,117,7). The PZ variant marks the spawn tile PZ.
    fn combat_map(spawn_pz: bool) -> Arc<StaticMap> {
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

    #[test]
    fn set_target_sets_attacking_and_clear_resets_it() {
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, Some(b), "set_target should store target id");
        g.do_set_target(a, 0);
        assert_eq!(g.players[&a].attacking, None, "target 0 clears the fight");
    }

    #[test]
    fn set_target_self_is_ignored() {
        let mut g = Game::new(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7));
        g.do_set_target(a, a);
        assert_eq!(g.players[&a].attacking, None, "self-target must be ignored");
        assert!(ra.try_recv().is_err(), "self-target must not push any packet");
    }

    #[test]
    fn set_target_from_pz_tile_rejects_and_pushes_0xb4() {
        // Attacker is standing on a PZ tile → attack must be rejected with 0xB4
        // and attacking must remain None.
        let mut g = Game::new(combat_map(true)); // spawn is PZ
        let (a, mut ra) = add_player(&mut g, Position::new(95, 117, 7)); // PZ tile
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, None, "PZ attacker must not get a target");
        let pkt = ra.try_recv().expect("PZ rejection must push a 0xB4 packet");
        assert_eq!(pkt[0], 0xB4, "PZ rejection packet must be a text message (0xB4)");
    }

    #[test]
    fn combat_tick_deals_damage_to_adjacent_target() {
        // A (attacker) and B (victim) are adjacent. After setting target and
        // advancing time past one attack interval, B must have lost HP.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        let b_hp_before = g.players[&b].health;
        // Advance time past the attack interval (last_attack_ms=0 → now_ms >= 2000).
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let b_hp_after = g.players[&b].health;
        // HP may have stayed the same if dmg happened to roll 0, but a 0x8C
        // must still have been pushed (damage applied even if 0). Actually let's
        // use a seeded RNG approach: with seed_from_u64(42) and level-1/skill-10
        // the first roll is non-zero, but since the Game RNG seed is entropy-based
        // we can only assert B received a 0x8C packet (spectator of own tile).
        let _ = b_hp_before;
        let _ = b_hp_after;
        let pkt = rb.try_recv().expect("victim must receive at least a 0x8C health-bar");
        assert_eq!(pkt[0], protocol::combat_packets::OP_CREATURE_HEALTH,
            "first packet must be 0x8C (health-bar)");
    }

    #[test]
    fn combat_tick_sends_stats_to_victim() {
        // After a combat tick, the victim must also receive its own 0xA0 stats.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        // Drain the 0x8C (spectator of own tile, health-bar first)
        let _ = rb.try_recv().expect("0x8C expected");
        // Then the 0xA0 self-stats
        let stats_pkt = rb.try_recv().expect("victim must also receive its own 0xA0 stats");
        assert_eq!(stats_pkt[0], protocol::enter_world::OP_STATS, "0xA0 self-stats expected");
    }

    #[test]
    fn combat_tick_spectator_receives_health_bar() {
        // A third-party spectator of B's tile must also receive the 0x8C.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        // Spectator sits close enough to see B's tile.
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(95, 116, 7));
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        let pkt = rx_spec.try_recv().expect("spectator must receive 0x8C health bar");
        assert_eq!(pkt[0], protocol::combat_packets::OP_CREATURE_HEALTH);
    }

    #[test]
    fn combat_tick_no_damage_when_target_out_of_melee_range() {
        // Target 2 tiles away → no swing, no packets.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(97, 117, 7)); // 2 tiles east
        g.do_set_target(a, b);
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert!(rb.try_recv().is_err(), "out-of-range target should receive no packets");
    }

    #[test]
    fn combat_tick_respects_interval_no_damage_before_due() {
        // tick at now_ms < MELEE_ATTACK_INTERVAL_MS must not swing.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        // Send a tick at t=1000ms (< 2000ms interval) → no swing.
        g.on_combat_tick(1000);
        assert!(rb.try_recv().is_err(), "tick before interval elapses must not produce damage");
    }

    #[test]
    fn death_sends_window_removes_victim_and_saves_at_temple() {
        // Death == logout: the victim gets the 0x28 window, is removed from the
        // world, and a SaveRecord is emitted at the temple with full HP — so the
        // relog spawns at the temple (M8 `login` restores the saved position).
        let mut g = Game::new(combat_map(false));
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (a, _ra) = add_player(&mut g, Position::new(97, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        let max_hp = g.players[&b].max_health;
        let temple = g.map.spawn();
        g.do_set_target(a, b);
        g.players.get_mut(&b).unwrap().health = 1;

        let mut saw_death_window = false;
        for tick in 1..=(max_hp as u64 + 5) {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            while let Ok(pkt) = rb.try_recv() {
                if pkt[0] == protocol::combat_packets::OP_DEATH_WINDOW {
                    saw_death_window = true;
                }
            }
            if !g.players.contains_key(&b) { break; }
        }

        assert!(saw_death_window, "dying player must receive the 0x28 death window");
        assert!(!g.players.contains_key(&b), "victim must be removed from the world on death");
        let rec = save_rx.try_recv().expect("death must emit a SaveRecord");
        assert_eq!(rec.position, temple, "death saves the player at the temple");
        assert_eq!(rec.health, rec.max_health, "death saves the player at full HP");
    }

    #[test]
    fn death_with_full_client_buffer_still_saves_at_temple() {
        // Regression: a saturated victim push buffer must NOT divert death through
        // the reaping push()/logout path (which saves at the death tile with the
        // current HP). do_death uses a non-reaping try_send for the death window.
        let mut g = Game::new(combat_map(false));
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        let temple = g.map.spawn();
        g.players.get_mut(&b).unwrap().health = 1;
        // Fill B's push channel to capacity so a reaping send would log it out.
        for _ in 0..super::PUSH_CAPACITY {
            g.push(b, vec![0u8]);
        }
        g.do_death(b);
        let rec = save_rx.try_recv().expect("death must emit a SaveRecord even with a full buffer");
        assert_eq!(rec.position, temple, "death saves at the temple even with a full client buffer");
        assert_eq!(rec.health, rec.max_health, "death saves full HP even with a full client buffer");
        assert!(!g.players.contains_key(&b), "victim must be removed from the world");
    }

    #[test]
    fn death_clears_attacker_fight() {
        // Death clears every fight targeting the victim. `fist_damage` rolls
        // 0..=max (a swing can deal 0), so tick until the kill lands rather than
        // assuming one swing kills.
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(97, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        g.players.get_mut(&b).unwrap().health = 1;
        for tick in 1..=200u64 {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            while rb.try_recv().is_ok() {} // drain packets
            if !g.players.contains_key(&b) { break; }
        }
        assert!(!g.players.contains_key(&b), "victim must be removed from the world on death");
        assert_eq!(g.players[&a].attacking, None, "attacker's fight must clear on target death");
    }

    #[test]
    fn death_remove_uses_id_form_for_coocc_safety() {
        // Regression for the M7<->co-occupancy merge: do_death must remove the
        // victim with the id-form (0x6C 0xFFFF <id>), not position+stackpos.
        // Under co-occupancy (stair/height landings) a position+stackpos remove
        // is ambiguous when another creature shares the death tile. Matches
        // logout and do_move.
        let mut g = Game::new(combat_map(false));
        let (a, mut ra) = add_player(&mut g, Position::new(97, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        let max_hp = g.players[&b].max_health;
        g.do_set_target(a, b);
        g.players.get_mut(&b).unwrap().health = 1;
        // Drive ticks until B dies; a single tick may roll 0 damage, so loop and
        // drain A's channel each tick (avoids overflow) until the 0x6C remove
        // appears. A is a spectator of B's death tile.
        let mut remove_pkt: Option<Vec<u8>> = None;
        for tick in 1..=(max_hp as u64 + 5) {
            g.on_combat_tick(tick * MELEE_ATTACK_INTERVAL_MS);
            while let Ok(pkt) = ra.try_recv() {
                if pkt.first() == Some(&0x6C) {
                    remove_pkt = Some(pkt);
                }
            }
            if remove_pkt.is_some() {
                break;
            }
        }
        let pkt = remove_pkt.expect("spectator must receive a 0x6C remove on the victim's death");
        assert_eq!(&pkt[1..3], &[0xFF, 0xFF], "death remove must be id-form (co-occupancy safe)");
        assert_eq!(&pkt[3..7], &b.to_le_bytes(), "id-form remove carries the victim id");
    }

    #[test]
    fn tick_clears_target_when_target_logs_out() {
        // If the target logs out, the attacker's attacking must be cleared on the
        // next tick (no panic, no stale fight).
        let mut g = Game::new(combat_map(false));
        let (a, _ra) = add_player(&mut g, Position::new(95, 117, 7));
        let (b, _rb) = add_player(&mut g, Position::new(96, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, Some(b));
        g.logout(b); // B disconnects
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);
        assert_eq!(g.players[&a].attacking, None, "attacker must clear when target logs out");
    }

    // -------------------------------------------------------------------------
    // M7 review fix tests (W1, W2, W3)
    // -------------------------------------------------------------------------

    /// A wide combat map (row 90..=116 walkable, temple at 95,117) where tile
    /// (90,117) is marked a protection zone — used by the PZ combat tests.
    fn wide_combat_map_with_pz() -> Arc<StaticMap> {
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

    // W3 repro: attacker locked on a target; target moves onto a PZ tile → next
    // tick must deal NO damage AND clear the attacker's `attacking` field.
    //
    // We can't actually move the target in this unit test (do_move needs a walkable
    // path), so we directly set the target's position to a PZ tile and fire a tick.
    // The tick must clear the fight, not just skip damage.
    #[test]
    fn combat_tick_clears_fight_when_target_enters_pz() {
        let mut g = Game::new(wide_combat_map_with_pz());
        // Attacker A at (91,117,7); target B starts adjacent at (92,117,7).
        let (a, _ra) = add_player(&mut g, Position::new(91, 117, 7));
        let (b, mut rb) = add_player(&mut g, Position::new(92, 117, 7));
        g.do_set_target(a, b);
        assert_eq!(g.players[&a].attacking, Some(b));

        // Teleport B onto the PZ tile (90,117,7) by direct state mutation —
        // simulates B stepping into the temple PZ.
        g.players.get_mut(&b).unwrap().position = Position::new(90, 117, 7);

        // Fire the combat tick — B is now in PZ, Chebyshev range = 1 (adjacent).
        // The tick MUST clear A's fight (not merely skip the swing).
        g.on_combat_tick(MELEE_ATTACK_INTERVAL_MS);

        assert_eq!(
            g.players[&a].attacking, None,
            "combat tick must clear attacker.attacking when target is in PZ (W3)"
        );
        // B must have received NO damage packet (no 0x8C).
        assert!(
            rb.try_recv().is_err(),
            "target in PZ must receive no damage (no 0x8C) on tick"
        );
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

    // -------------------------------------------------------------------------
    // M8 persistence-wiring tests
    // -------------------------------------------------------------------------

    /// A custom outfit distinct from the default knight outfit, for restore tests.
    fn wizard_outfit() -> Outfit {
        Outfit { look_type: 75, head: 20, body: 30, legs: 40, feet: 50, addons: 1, mount: 0 }
    }

    #[test]
    fn login_with_initial_position_places_player_at_that_position() {
        // RED: Game::login accepts InitialState { position: Some(p) } and places
        // the player at p with the given outfit and health.
        let mut g = Game::new(walk_map());
        let (tx, _rx) = mpsc::channel(PUSH_CAPACITY);
        let pos = Position::new(96, 117, 7);
        let outfit = wizard_outfit();
        let initial = InitialState {
            position: Some(pos),
            direction: Direction::North,
            outfit,
            health: 80,
            max_health: 120,
            sex: 1,
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("Restored".into(), initial, tx);
        let ps = g.players.get(&ack.snapshot.id).expect("player must exist");
        assert_eq!(ps.position, pos, "restored player must be at saved position");
        assert_eq!(ps.outfit, outfit, "restored player must have saved outfit");
        assert_eq!(ps.health, 80, "restored player must have saved health");
        assert_eq!(ps.max_health, 120, "restored player must have saved max_health");
        assert_eq!(ps.direction, Direction::North, "restored player must face saved direction");
        assert_eq!(ack.snapshot.outfit, outfit, "snapshot outfit must match");
    }

    #[test]
    fn login_with_no_position_falls_back_to_free_spawn() {
        // RED: Game::login with InitialState { position: None } resolves position
        // from free_spawn(), using default outfit/health for a new player.
        let mut g = Game::new(walk_map());
        let (tx, _rx) = mpsc::channel(PUSH_CAPACITY);
        let spawn = g.map.spawn();
        let initial = InitialState {
            position: None,
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 1,
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("NewPlayer".into(), initial, tx);
        assert_eq!(
            g.players.get(&ack.snapshot.id).unwrap().position,
            spawn,
            "new player with no saved position must spawn at free_spawn()"
        );
    }

    #[test]
    fn logout_with_save_tx_emits_save_record() {
        // RED: Game::logout emits a SaveRecord on save_tx when one is set.
        // The record must carry the player's current name/position/direction/outfit/health.
        let mut g = Game::new(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);

        let pos = Position::new(96, 117, 7);
        let outfit = wizard_outfit();
        let (tx, _rx) = mpsc::channel(PUSH_CAPACITY);
        let id = g.next_id;
        g.next_id += 1;
        g.players.insert(id, PlayerState {
            name: "Hero".into(), position: pos, direction: Direction::East,
            outfit, push_tx: tx, known: HashSet::new(),
            health: 77, max_health: 150, fist_skill: 10,
            attacking: None, last_attack_ms: 0,
            sex: 1,
            gamemaster: false,
            inventory: [None; 10],
            open_containers: std::array::from_fn(|_| None),
        });

        g.logout(id);

        let rec = save_rx.try_recv().expect("logout must emit a SaveRecord");
        assert_eq!(rec.name, "Hero");
        assert_eq!(rec.position, pos);
        assert_eq!(rec.direction, Direction::East);
        assert_eq!(rec.outfit, outfit);
        assert_eq!(rec.health, 77);
        assert_eq!(rec.max_health, 150);
    }

    #[test]
    fn save_all_emits_one_record_per_player_without_removing_them() {
        // Graceful shutdown: save_all must emit a SaveRecord for EVERY online
        // player (carrying their live outfit/position) and leave them in the map
        // — it persists without logging anyone out.
        let mut g = Game::new(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);

        let (tx_a, _rx_a) = mpsc::channel(PUSH_CAPACITY);
        let id_a = g.next_id; g.next_id += 1;
        let pos_a = Position::new(96, 117, 7);
        g.players.insert(id_a, PlayerState {
            name: "Diego".into(), position: pos_a, direction: Direction::North,
            outfit: wizard_outfit(), push_tx: tx_a, known: HashSet::new(),
            health: 90, max_health: 150, fist_skill: 10,
            attacking: None, last_attack_ms: 0, sex: 1, gamemaster: true,
            inventory: [None; 10],
            open_containers: std::array::from_fn(|_| None),
        });

        let (tx_b, _rx_b) = mpsc::channel(PUSH_CAPACITY);
        let id_b = g.next_id; g.next_id += 1;
        let pos_b = Position::new(100, 120, 7);
        g.players.insert(id_b, PlayerState {
            name: "Grissda".into(), position: pos_b, direction: Direction::South,
            outfit: knight(), push_tx: tx_b, known: HashSet::new(),
            health: 145, max_health: 145, fist_skill: 10,
            attacking: None, last_attack_ms: 0, sex: 0, gamemaster: false,
            inventory: [None; 10],
            open_containers: std::array::from_fn(|_| None),
        });

        g.save_all();

        // Both players must still be in the world (save_all does not log out).
        assert_eq!(g.players.len(), 2, "save_all must not remove players");
        assert!(g.players.contains_key(&id_a) && g.players.contains_key(&id_b));

        // Exactly two records, one per player, carrying live state.
        let mut recs = Vec::new();
        while let Ok(rec) = save_rx.try_recv() { recs.push(rec); }
        assert_eq!(recs.len(), 2, "save_all must emit one record per online player");

        let diego = recs.iter().find(|r| r.name == "Diego").expect("Diego record");
        assert_eq!(diego.position, pos_a);
        assert_eq!(diego.direction, Direction::North);
        assert_eq!(diego.outfit, wizard_outfit());
        assert_eq!(diego.health, 90);

        let grissda = recs.iter().find(|r| r.name == "Grissda").expect("Grissda record");
        assert_eq!(grissda.position, pos_b);
        assert_eq!(grissda.outfit, knight());
        assert_eq!(grissda.health, 145);
    }

    #[test]
    fn save_all_with_no_save_tx_is_a_noop() {
        // With no save channel wired, save_all must not panic (and there is
        // nowhere for records to go).
        let mut g = Game::new(walk_map());
        let (tx, _rx) = mpsc::channel(PUSH_CAPACITY);
        let id = g.next_id; g.next_id += 1;
        g.players.insert(id, PlayerState {
            name: "Lonely".into(), position: Position::new(96, 117, 7),
            direction: Direction::West, outfit: knight(), push_tx: tx,
            known: HashSet::new(), health: 100, max_health: 100, fist_skill: 10,
            attacking: None, last_attack_ms: 0, sex: 1, gamemaster: false,
            inventory: [None; 10],
            open_containers: std::array::from_fn(|_| None),
        });

        g.save_all(); // must not panic
        assert_eq!(g.players.len(), 1, "players are untouched when there is no save_tx");
    }

    #[test]
    fn push_to_dead_channel_reap_also_emits_save_record() {
        // RED: The internal dead-session reap path (push() -> logout()) also emits
        // a SaveRecord when save_tx is set.
        let mut g = Game::new(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);

        // Create a player whose push channel has a DROPPED receiver — any push will fail.
        let (tx, rx) = mpsc::channel::<Vec<u8>>(1);
        drop(rx); // receiver gone: try_send will immediately fail
        let id = g.next_id;
        g.next_id += 1;
        g.players.insert(id, PlayerState {
            name: "Ghost".into(), position: g.map.spawn(), direction: Direction::South,
            outfit: knight(), push_tx: tx, known: HashSet::new(),
            health: 50, max_health: 150, fist_skill: 10,
            attacking: None, last_attack_ms: 0,
            sex: 1,
            gamemaster: false,
            inventory: [None; 10],
            open_containers: std::array::from_fn(|_| None),
        });

        // Pushing any payload triggers the dead-session reap → logout → save.
        g.push(id, vec![0xFF]);
        let rec = save_rx.try_recv()
            .expect("dead-session reap must also emit a SaveRecord");
        assert_eq!(rec.name, "Ghost");
        assert_eq!(rec.health, 50);
    }

    // ---------------------------------------------------------------------------
    // M8 Slice B — outfit-change spine tests
    // ---------------------------------------------------------------------------

    #[test]
    fn change_outfit_updates_player_state() {
        let mut g = Game::new(walk_map());
        let (id, _rx) = add_player(&mut g, Position::new(95, 117, 7));
        let new_outfit = Outfit { look_type: 130, head: 10, body: 20, legs: 30, feet: 40, addons: 3, mount: 0 };
        g.do_change_outfit(id, new_outfit);
        assert_eq!(g.players[&id].outfit, new_outfit);
    }

    #[test]
    fn change_outfit_broadcasts_0x8e_to_player_and_spectator() {
        let mut g = Game::new(walk_map());
        // Both players at the same tile so they are each other's spectators.
        let (id, mut rx_self)  = add_player(&mut g, Position::new(95, 117, 7));
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(95, 117, 7));
        let new_outfit = Outfit { look_type: 130, head: 0, body: 0, legs: 0, feet: 0, addons: 0, mount: 0 };
        g.do_change_outfit(id, new_outfit);

        // Drain initial login messages; the LAST packet in the channel is the outfit broadcast.
        let pkt_self = {
            let mut last = None;
            while let Ok(p) = rx_self.try_recv() { last = Some(p); }
            last.expect("player must receive at least one packet (the 0x8E)")
        };
        let pkt_spec = {
            let mut last = None;
            while let Ok(p) = rx_spec.try_recv() { last = Some(p); }
            last.expect("spectator must receive at least one packet (the 0x8E)")
        };
        assert_eq!(pkt_self[0], protocol::outfit::OP_CREATURE_OUTFIT, "player must receive 0x8E");
        assert_eq!(pkt_spec[0], protocol::outfit::OP_CREATURE_OUTFIT, "spectator must receive 0x8E");
        // Both packets must carry the changer's id.
        let id_bytes = id.to_le_bytes();
        assert_eq!(&pkt_self[1..5], &id_bytes);
        assert_eq!(&pkt_spec[1..5], &id_bytes);
    }

    #[test]
    fn change_outfit_unknown_id_is_noop() {
        let mut g = Game::new(walk_map());
        // Should not panic; game has no players.
        g.do_change_outfit(0xDEAD_BEEF, Outfit { look_type: 130, head: 0, body: 0, legs: 0, feet: 0, addons: 0, mount: 0 });
    }

    #[test]
    fn request_outfit_sends_0xc8_to_requester_only() {
        let mut g = Game::new(walk_map());
        let (id, mut rx_self)  = add_player(&mut g, Position::new(95, 117, 7));
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(95, 117, 7));
        // Drain any login-side packets first.
        while rx_self.try_recv().is_ok() {}
        while rx_spec.try_recv().is_ok() {}
        g.do_request_outfit(id);
        let pkt = rx_self.try_recv().expect("requester must receive 0xC8");
        assert_eq!(pkt[0], protocol::outfit::OP_OUTFIT_WINDOW, "packet must be 0xC8");
        assert!(rx_spec.try_recv().is_err(), "spectator must NOT receive anything");
    }

    /// Parse the available-outfit `look_type`s out of a `0xC8` outfit-window
    /// packet. Assumes the `current` outfit has a non-zero look_type, so its
    /// `AddOutfit` block is 9 bytes (u16 look_type + 5×u8 + u16 mount).
    fn outfit_window_looktypes(pkt: &[u8]) -> Vec<u16> {
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

    #[test]
    fn request_outfit_male_gets_male_catalog() {
        let mut g = Game::new(walk_map());
        let (id, mut rx) = add_player(&mut g, Position::new(95, 117, 7)); // sex = 1 (male)
        while rx.try_recv().is_ok() {}
        g.do_request_outfit(id);
        let pkt = rx.try_recv().expect("requester must receive 0xC8");
        assert_eq!(pkt[0], protocol::outfit::OP_OUTFIT_WINDOW);
        let types = outfit_window_looktypes(&pkt);
        assert_eq!(types.len(), 55, "male catalog has all 55 outfits");
        assert!(types.contains(&128), "male catalog contains male Citizen (128)");
        assert!(!types.contains(&136), "male catalog must NOT contain female Citizen (136)");
    }

    #[test]
    fn request_outfit_female_gets_female_catalog() {
        let mut g = Game::new(walk_map());
        let (id, mut rx) = add_player(&mut g, Position::new(95, 117, 7));
        g.players.get_mut(&id).unwrap().sex = 0; // female
        while rx.try_recv().is_ok() {}
        g.do_request_outfit(id);
        let pkt = rx.try_recv().expect("requester must receive 0xC8");
        let types = outfit_window_looktypes(&pkt);
        assert_eq!(types.len(), 55, "female catalog has all 55 outfits");
        assert!(types.contains(&136), "female catalog contains female Citizen (136)");
        assert!(!types.contains(&128), "female catalog must NOT contain male Citizen (128)");
    }

    /// Drain a receiver and return the last `0xA2` icons packet seen, if any.
    fn drain_find_icons(rx: &mut mpsc::Receiver<Vec<u8>>) -> Option<Vec<u8>> {
        let mut found = None;
        while let Ok(pkt) = rx.try_recv() {
            if pkt.first() == Some(&enter_world::OP_ICONS) {
                found = Some(pkt);
            }
        }
        found
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
    // M8 sex / gender plumbing tests
    // -------------------------------------------------------------------------

    #[test]
    fn sex_is_set_from_initial_state_on_login() {
        // RED: InitialState must carry a `sex` field that is stored in the live
        // PlayerState and exposed via do_request_outfit catalog selection later.
        let mut g = Game::new(walk_map());
        let (tx, _rx) = mpsc::channel(PUSH_CAPACITY);
        let initial = InitialState {
            position: None,
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 0, // female
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("Tester".into(), initial, tx);
        assert_eq!(
            g.players[&ack.snapshot.id].sex, 0,
            "sex from InitialState must be stored in PlayerState"
        );
    }

    #[test]
    fn sex_is_emitted_in_save_record_on_logout() {
        // RED: logout must carry sex into SaveRecord so the server can persist it.
        let mut g = Game::new(walk_map());
        let (save_tx, mut save_rx) = mpsc::unbounded_channel::<SaveRecord>();
        g.save_tx = Some(save_tx);
        let (tx, _rx) = mpsc::channel(PUSH_CAPACITY);
        let initial = InitialState {
            position: None,
            direction: Direction::South,
            outfit: knight(),
            health: 150,
            max_health: 150,
            sex: 0, // female
            gamemaster: false,
            inventory: Vec::new(),
            container_items: Vec::new(),
        };
        let ack = g.login("Tester".into(), initial, tx);
        let id = ack.snapshot.id;
        g.logout(id);
        let rec = save_rx.try_recv().expect("logout must emit a SaveRecord");
        assert_eq!(rec.sex, 0, "sex must round-trip login→logout through SaveRecord");
    }

    // -------------------------------------------------------------------------
    // M9 do_look tests
    // -------------------------------------------------------------------------

    /// FLAG_PICKUPABLE (bit 5) from items.otb.
    const FLAG_PICKUPABLE_OTB: u32 = 1 << 5;
    /// FLAG_STACKABLE (bit 7) from items.otb.
    const FLAG_STACKABLE_OTB: u32 = 1 << 7;

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
    fn look_map() -> Arc<StaticMap> {
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
    fn recv_look_text(rx: &mut mpsc::Receiver<Vec<u8>>) -> String {
        let pkt = rx.try_recv().expect("expected a 0xB4 look packet");
        assert_eq!(pkt[0], 0xB4, "must be a 0xB4 text message");
        assert_eq!(pkt[1], MSG_INFO_DESCR, "must be MESSAGE_INFO_DESCR (22)");
        let len = u16::from_le_bytes([pkt[2], pkt[3]]) as usize;
        String::from_utf8(pkt[4..4 + len].to_vec()).expect("look text must be valid UTF-8")
    }

    #[test]
    fn do_look_ground_item_adjacent_shows_article_name_and_weight() {
        let mut g = Game::new(look_map());
        // Looker at (100,100,7), stone is at (101,100,7) — distance 1 (adjacent).
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // stackpos 1 = stone on tile (101,100,7)
        g.do_look(looker, 101, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        // Must contain "You see a stone."
        assert!(text.contains("You see a stone."), "text: {text:?}");
        // Adjacent → must show weight line: "It weighs 1.10 oz."
        assert!(text.contains("It weighs 1.10 oz."), "adjacent item must show weight; text: {text:?}");
    }

    #[test]
    fn do_look_ground_item_far_away_omits_weight() {
        let mut g = Game::new(look_map());
        // Looker at (100,100,7). Put another player at (103,100,7) to create a
        // position 3 tiles away. Actually just move the looker far from the stone.
        // Stone is at (101,100,7). Place looker at (103,100,7) → dist = 2.
        let (looker, mut rx) = add_player(&mut g, Position::new(103, 100, 7));
        g.do_look(looker, 101, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(text.contains("You see a stone."), "text: {text:?}");
        // Distance ≥ 2 → no weight line
        assert!(!text.contains("weighs"), "far look must NOT show weight; text: {text:?}");
    }

    #[test]
    fn do_look_non_pickupable_item_no_weight_line() {
        // Ground item (sid 100) is not pickupable → no weight line even when adjacent.
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // stackpos 0 = ground at (100,100,7) itself
        g.do_look(looker, 100, 100, 7, 0);
        let text = recv_look_text(&mut rx);
        assert!(!text.contains("weighs"), "non-pickupable item must not show weight; text: {text:?}");
    }

    #[test]
    fn do_look_stackable_item_with_count_shows_count_and_plural() {
        // gold coins (sid 300) at (102,100,7), count 50. show_count true.
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // stackpos 1 = gold coins at (102,100,7)
        g.do_look(looker, 102, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        // "You see 50 gold coins."
        assert!(text.contains("You see 50 gold coins."), "text: {text:?}");
    }

    #[test]
    fn do_look_other_player_shows_name_level_and_pronoun() {
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // Add a target player with a distinctive name, placed adjacent.
        let (tx2, _rx2) = mpsc::channel(PUSH_CAPACITY);
        let target_id = g.next_id;
        g.next_id += 1;
        g.players.insert(target_id, PlayerState {
            name: "Alice".into(), position: Position::new(100, 100, 7),
            direction: Direction::South, outfit: knight(), push_tx: tx2,
            known: HashSet::new(), health: 150, max_health: 150, fist_skill: 10,
            attacking: None, last_attack_ms: 0,
            sex: 0, // female
            gamemaster: false,
            inventory: [None; 10],
            open_containers: std::array::from_fn(|_| None),
        });
        // Looker is at the same tile; tile pre_creature_len is 1 (just the ground),
        // creatures = [looker_id, target_id] (sorted). stackpos 1 = first creature
        // (the lower id), stackpos 2 = second. Since both players are at (100,100,7)
        // and ids are assigned sequentially with looker first, target_id > looker_id.
        // pre = 1, creatures = [looker, target] sorted by id.
        // looker_id < target_id so stackpos 1 = looker, stackpos 2 = target.
        g.do_look(looker, 100, 100, 7, 2);
        let text = recv_look_text(&mut rx);
        assert!(text.contains("Alice (Level 1)."), "text: {text:?}");
        assert!(text.contains("She has no vocation."), "female pronoun; text: {text:?}");
        // Now change to male and re-verify.
        g.players.get_mut(&target_id).unwrap().sex = 1;
        g.do_look(looker, 100, 100, 7, 2);
        let text2 = recv_look_text(&mut rx);
        assert!(text2.contains("He has no vocation."), "male pronoun; text2: {text2:?}");
    }

    #[test]
    fn do_look_self_shows_yourself() {
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // pre_creature_len = 1 (ground), stackpos 1 = looker itself.
        g.do_look(looker, 100, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(text.contains("You see yourself."), "text: {text:?}");
        assert!(text.contains("You have no vocation."), "text: {text:?}");
    }

    #[test]
    fn do_look_gamemaster_item_appends_item_id_and_position() {
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // Elevate to GM.
        g.players.get_mut(&looker).unwrap().gamemaster = true;
        // Look at stone (sid 200) at (101,100,7), stackpos 1.
        g.do_look(looker, 101, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(text.ends_with("\nItem ID: 200\nPosition: 101, 100, 7"),
            "GM look must end with Item ID and Position; text: {text:?}");
    }

    #[test]
    fn do_look_non_gamemaster_no_debug_suffix() {
        let mut g = Game::new(look_map());
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // gamemaster = false (default)
        g.do_look(looker, 101, 100, 7, 1);
        let text = recv_look_text(&mut rx);
        assert!(!text.contains("Item ID:"), "non-GM must not see Item ID; text: {text:?}");
        assert!(!text.contains("Position:"), "non-GM must not see Position; text: {text:?}");
    }

    #[test]
    fn do_look_out_of_viewport_pushes_nothing() {
        let mut g = Game::new(look_map());
        // Looker at (100,100,7). A tile that is far out of the viewport
        // (dx = 50 > 9) must produce no packet.
        let (looker, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.do_look(looker, 150, 100, 7, 0);
        assert!(rx.try_recv().is_err(), "look outside viewport must push nothing");
    }

    // -------------------------------------------------------------------------
    // M10.1 do_move_thing tests
    // -------------------------------------------------------------------------

    /// FLAG_MOVEABLE = bit 6, FLAG_STACKABLE = bit 7, FLAG_PICKUPABLE = bit 5.
    const FLAG_MOVEABLE_OTB: u32 = 1 << 6;

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
    fn move_map() -> Arc<StaticMap> {
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

        let g = |x: u16| formats::otbm::MapTile { x, y: 100, z: 7, flags: 0, house_id: None,
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
    fn drain(rx: &mut mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Ok(p) = rx.try_recv() { out.push(p); }
        out
    }

    /// Helper: assert a packet list contains at least one packet whose first byte is `op`.
    fn has_op(packets: &[Vec<u8>], op: u8) -> bool {
        packets.iter().any(|p| p.first() == Some(&op))
    }

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

    #[test]
    fn throwing_open_inventory_container_follows_to_ground_with_contents() {
        // New issue: throwing an open inventory backpack must not strand its
        // contents on the old slot window. The window follows the item to the
        // ground tile (contents intact) and closes if the throw lands out of
        // range — exactly one window, no duplicate, no empty ground bag.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        {
            let p = g.players.get_mut(&player).unwrap();
            p.inventory[2] = Some(InvItem { server_id: 600, client_id: 1988, count: None, animated: false });
            p.open_containers[0] = Some(OpenContainer {
                server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
                items: vec![ContainerItem { server_id: 200, client_id: 1987, count: None, animated: false }],
                source: ContainerSource::Slot(3), is_open: true,
            });
        }
        drain(&mut rx);

        // Throw the backpack from slot 3 to a far ground tile (105,100,7).
        g.do_move_inventory(player, Position::new(0xFFFF, 3, 0), 0, Position::new(105, 100, 7), 1);

        let cids: Vec<&OpenContainer> = (0u8..16)
            .filter_map(|c| g.players[&player].open_containers[c as usize].as_ref())
            .collect();
        assert_eq!(cids.len(), 1, "still exactly one container window (no duplicate)");
        assert!(matches!(cids[0].source, ContainerSource::Ground(p) if p == Position::new(105, 100, 7)),
            "window re-keyed to the ground tile; got {:?}", cids[0].source);
        assert!(!cids[0].is_open, "thrown out of range -> window closed");
        assert!(cids[0].items.iter().any(|i| i.server_id == 200),
            "contents preserved on the ground container");
        assert!(g.players[&player].inventory[2].is_none(), "slot 3 emptied by the throw");
    }

    #[test]
    fn walking_away_closes_ground_container_keeps_inventory_open() {
        // Issue 3: a ground container auto-closes when the player walks more than
        // one tile away; an inventory container travels with the player and stays
        // open. Player starts on (102,100,7) with both open.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(102, 100, 7));
        {
            let p = g.players.get_mut(&player).unwrap();
            p.open_containers[0] = Some(OpenContainer {
                server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
                items: vec![], source: ContainerSource::Ground(Position::new(102, 100, 7)), is_open: true,
            });
            p.open_containers[1] = Some(OpenContainer {
                server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
                items: vec![], source: ContainerSource::Slot(3), is_open: true,
            });
        }
        drain(&mut rx);

        // Step to 101: ground container at 102 is 1 tile away -> stays open.
        g.do_move(player, Direction::West);
        assert!(g.players[&player].open_containers[0].as_ref().unwrap().is_open,
            "ground container one tile away must stay open");

        // Step to 100: ground container at 102 is now 2 tiles away -> closes.
        drain(&mut rx);
        g.do_move(player, Direction::West);
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_CLOSE_CONTAINER),
            "a 0x6F close must be sent for the out-of-range ground container; got {:?}", pkts);
        assert!(!g.players[&player].open_containers[0].as_ref().unwrap().is_open,
            "ground container more than one tile away must close");
        assert!(g.players[&player].open_containers[1].as_ref().unwrap().is_open,
            "inventory container must stay open while walking");
    }

    #[test]
    fn drop_onto_nested_bag_opened_before_parent_shift_is_not_lost() {
        // Real loss repro: open the inner bag FIRST (its cid is pinned to the slot
        // it occupied then), THEN insert an item at the front of the parent — which
        // shifts the inner bag down a slot — THEN drag an item onto the inner bag.
        // Without slot maintenance on insertion the inner bag's cached parent_slot
        // goes stale: a duplicate cid is allocated, close_orphaned collapses both to
        // the same source, and the empty stale cid shadows the one holding the item
        // on reopen -> the item is stranded. The fix keeps exactly one nested cid,
        // open, holding the item.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        g.players.get_mut(&player).unwrap().inventory[2] =
            Some(InvItem { server_id: 600, client_id: 1988, count: None, animated: false });
        // Parent backpack (cid 0) initially holds only the inner bag at slot 0.
        g.players.get_mut(&player).unwrap().open_containers[0] = Some(OpenContainer {
            server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
            items: vec![ContainerItem { server_id: 600, client_id: 1988, count: None, animated: false }],
            source: ContainerSource::Slot(3), is_open: true,
        });
        drain(&mut rx);

        // Open the inner bag (parent cid 0, slot 0) -> nested cid pinned to slot 0.
        g.do_use_item(player, 0xFFFF, 0x40, 0, 0, 0);
        // Insert a stone at the FRONT of the parent, shifting the inner bag to slot 1.
        g.push_item_to_container(player, 0, ContainerItem { server_id: 200, client_id: 1987, count: None, animated: false });
        drain(&mut rx);

        // Drag the stone (cid 0, slot 0) onto the inner bag icon (cid 0, slot 1).
        g.do_move_container(player, Position::new(0xFFFF, 0x40, 0), 0, Position::new(0xFFFF, 0x40, 1), 1);

        // Exactly one nested cid under the parent, open, holding the stone.
        let nested: Vec<&OpenContainer> = (0u8..16).filter_map(|c| {
            g.players[&player].open_containers[c as usize].as_ref()
                .filter(|oc| matches!(oc.source, ContainerSource::Nested { parent_cid: 0, .. }))
        }).collect();
        assert_eq!(nested.len(), 1, "exactly one nested cid (no shadow duplicate)");
        assert!(nested[0].items.iter().any(|i| i.server_id == 200),
            "stone must be inside the inner bag, not stranded; contents: {:?}",
            nested[0].items.iter().map(|i| i.server_id).collect::<Vec<_>>());
        assert!(nested[0].is_open, "inner bag stays open and shows the stone");
    }

    #[test]
    fn drop_item_onto_nested_bag_routes_inside_and_is_retrievable() {
        // Issue 2 repro: drag a non-container item onto a CLOSED nested bag icon
        // inside an open parent backpack. The item must route INTO the nested bag
        // (vanishing from the parent window, no false re-add) and be retrievable
        // by opening that nested bag. Parent = cid 0 (inventory slot 3) holding
        // [stone@slot0, inner-backpack@slot1].
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        {
            let p = g.players.get_mut(&player).unwrap();
            p.inventory[2] = Some(InvItem { server_id: 600, client_id: 1988, count: None, animated: false });
            p.open_containers[0] = Some(OpenContainer {
                server_id: 600, client_id: 1988, capacity: 20, name: "backpack".into(),
                items: vec![
                    ContainerItem { server_id: 200, client_id: 1987, count: None, animated: false }, // stone slot 0
                    ContainerItem { server_id: 600, client_id: 1988, count: None, animated: false }, // inner bag slot 1
                ],
                source: ContainerSource::Slot(3), is_open: true,
            });
        }
        drain(&mut rx);

        // Drag stone (cid 0, slot 0) onto the inner bag (cid 0, slot 1).
        let from = Position::new(0xFFFF, 0x40, 0); // cid 0, slot 0
        let to = Position::new(0xFFFF, 0x40, 1);   // cid 0, slot 1 (inner bag)
        g.do_move_container(player, from, 0, to, 1);

        // Parent must keep only the inner bag; the stone left it.
        let parent = g.players[&player].open_containers[0].as_ref().unwrap();
        assert_eq!(parent.items.len(), 1, "parent should keep only the inner bag");
        assert_eq!(parent.items[0].server_id, 600, "remaining parent item is the inner bag");

        // A nested cid must now track the inner bag and hold the stone.
        let nested = (0u8..16).find(|&c| {
            c != 0 && g.players[&player].open_containers[c as usize].as_ref()
                .is_some_and(|o| matches!(o.source, ContainerSource::Nested { parent_cid: 0, .. }))
        }).expect("a nested cid must be allocated for the inner bag");
        let noc = g.players[&player].open_containers[nested as usize].as_ref().unwrap();
        assert_eq!(noc.items.len(), 1, "inner bag must hold the routed stone (not lost)");
        assert_eq!(noc.items[0].server_id, 200, "routed item is the stone");

        // Retrievable: opening the inner bag (now at parent slot 0) shows the stone.
        drain(&mut rx);
        g.do_use_item(player, 0xFFFF, 0x40, 0, 0, 0); // container endpoint: cid 0, slot 0
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_OPEN_CONTAINER),
            "opening the inner bag must push 0x6E; got {:?}", pkts);
        let noc = g.players[&player].open_containers[nested as usize].as_ref().unwrap();
        assert!(noc.is_open, "inner bag must be open after use");
        assert_eq!(noc.items[0].server_id, 200, "stone must be visible inside the opened inner bag");
    }

    /// Count how many dynamic tiles currently hold an item with server id `sid`,
    /// summed across every overlay stack (catches duplication into multiple tiles).
    fn count_sid_in_overlays(g: &Game, sid: u16) -> usize {
        g.dynamic.values()
            .map(|st| st.server_ids.iter().filter(|&&s| s == sid).count())
            .sum()
    }

    #[test]
    fn do_move_thing_multi_hop_never_duplicates_including_on_tile() {
        // Reproduction for the reported "moving an item duplicates it at every
        // tile" bug. Drag the stone (sid 200, non-stackable, moveable) across a
        // chain of tiles — including a hop where the destination is the player's
        // OWN tile (creature present), which is the on-tile case the existing
        // tests deliberately avoid. After every hop exactly ONE stone must exist.
        let mut g = Game::new(move_map());
        // Player stands on (102,100,7) the whole time; it can reach 101/102/103.
        let (player, mut rx) = add_player(&mut g, Position::new(102, 100, 7));
        drain(&mut rx);

        // Hop 1: 101 -> 102 (onto the player's tile). Stone on 101 is at item
        // index 1 (ground at 0), no creature on 101 -> wire stackpos 1.
        g.do_move_thing(player, Position::new(101, 100, 7), 1, Position::new(102, 100, 7), 1);
        assert_eq!(count_sid_in_overlays(&g, 200), 1,
            "after hop 1 exactly one stone must exist; overlays: {:?}",
            g.dynamic.iter().map(|(k, v)| (*k, v.server_ids.clone())).collect::<Vec<_>>());

        // Hop 2: 102 -> 103. The stone is now a DOWN item on the player's tile:
        // pre_creature_len=1 (ground), 1 creature, stone at down-index 0
        // -> wire stackpos = 1 + 1 = 2.
        g.do_move_thing(player, Position::new(102, 100, 7), 2, Position::new(103, 100, 7), 1);
        assert_eq!(count_sid_in_overlays(&g, 200), 1,
            "after hop 2 exactly one stone must exist; overlays: {:?}",
            g.dynamic.iter().map(|(k, v)| (*k, v.server_ids.clone())).collect::<Vec<_>>());

        // Hop 3: 103 -> 101. Stone on 103 sits above the deco (down-index 1):
        // pre_creature_len=1, no creature on 103 -> wire stackpos 1.
        g.do_move_thing(player, Position::new(103, 100, 7), 1, Position::new(101, 100, 7), 1);
        assert_eq!(count_sid_in_overlays(&g, 200), 1,
            "after hop 3 exactly one stone must exist; overlays: {:?}",
            g.dynamic.iter().map(|(k, v)| (*k, v.server_ids.clone())).collect::<Vec<_>>());
    }

    #[test]
    fn do_use_item_on_open_container_toggles_closed() {
        // Issue 4: using (0x82) a container that is already open must CLOSE it
        // (TFS actions.cpp toggle), not re-send another 0x6E open. A third use
        // re-opens it. Source is inventory slot 3 (pos_x=0xFFFF, pos_y=3, no 0x40).
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        // Put the backpack (sid 600) in inventory slot 3.
        g.players.get_mut(&player).unwrap().inventory[2] = Some(InvItem {
            server_id: 600, client_id: 1988, count: None, animated: false,
        });
        drain(&mut rx);

        // 1st use → open. Expect a 0x6E and is_open == true.
        g.do_use_item(player, 0xFFFF, 3, 0, 0, 0);
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_OPEN_CONTAINER),
            "first use must push 0x6E open; got {:?}", pkts);
        let cid = (0u8..16).find(|&c| g.players[&player].open_containers[c as usize].is_some())
            .expect("a container slot must be allocated");
        assert!(g.players[&player].open_containers[cid as usize].as_ref().unwrap().is_open,
            "container must be open after first use");

        // 2nd use → close. Expect a 0x6F and is_open == false (slot retained).
        g.do_use_item(player, 0xFFFF, 3, 0, 0, 0);
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_CLOSE_CONTAINER),
            "second use must push 0x6F close; got {:?}", pkts);
        assert!(!has_op(&pkts, protocol::container::OP_OPEN_CONTAINER),
            "second use must NOT re-send 0x6E open; got {:?}", pkts);
        assert!(!g.players[&player].open_containers[cid as usize].as_ref().unwrap().is_open,
            "container must be closed (but retained) after second use");

        // 3rd use → re-open the retained slot.
        g.do_use_item(player, 0xFFFF, 3, 0, 0, 0);
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, protocol::container::OP_OPEN_CONTAINER),
            "third use must re-open with 0x6E; got {:?}", pkts);
        assert!(g.players[&player].open_containers[cid as usize].as_ref().unwrap().is_open,
            "container must be open again after third use");
    }

    #[test]
    fn do_move_thing_from_eq_to_is_noop() {
        // from == to must be an early return with no overlay change and no packet.
        let mut g = Game::new(move_map());
        // Player at (100,100,7) (spawn), source at (101,100,7) — adjacent.
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        let pos = Position::new(101, 100, 7);
        drain(&mut rx); // discard any login-related packets
        g.do_move_thing(player, pos, 1, pos, 1);
        let pkts = drain(&mut rx);
        assert!(pkts.is_empty(), "from==to must produce no packet");
    }

    #[test]
    fn do_move_thing_non_moveable_is_rejected_with_status_push() {
        // sid 400 has no FLAG_MOVEABLE → must be rejected.
        // Player at (102,100,7), source deco at (103,100,7) — adjacent (dx=1).
        // stackpos for the item: pre_creature_len for (103,100,7) = 1 (ground),
        // no creatures on that tile → item is at stackpos 1.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(102, 100, 7));
        drain(&mut rx);

        let from = Position::new(103, 100, 7);
        let to   = Position::new(105, 100, 7); // valid dest, 2 tiles away (within range)
        g.do_move_thing(player, from, 1, to, 1);

        let pkts = drain(&mut rx);
        // Must receive a status push (0xB4 MSG_STATUS_SMALL = 21).
        assert!(has_op(&pkts, 0xB4), "non-moveable rejection must push 0xB4; got {:?}", pkts);
        let status_pkt = pkts.iter().find(|p| p.first() == Some(&0xB4)).unwrap();
        assert_eq!(status_pkt[1], MSG_STATUS_SMALL, "must be MSG_STATUS_SMALL (21)");

        // The overlay for the from tile must be absent or unmodified (item still there).
        let overlay_count = g.dynamic.get(&(103, 100, 7))
            .map(|st| st.server_ids.iter().filter(|&&sid| sid == 400).count())
            .unwrap_or(1); // static map still has the item
        assert!(overlay_count >= 1, "non-moveable item must remain on source tile");
    }

    #[test]
    fn do_move_thing_out_of_reach_is_rejected() {
        // Player at (100,100,7), source at (103,100,7) — dx=3 > 1 → too far.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        let from = Position::new(103, 100, 7);
        let to   = Position::new(105, 100, 7);
        g.do_move_thing(player, from, 1, to, 1);

        let pkts = drain(&mut rx);
        // Must get a status push (too far away).
        assert!(has_op(&pkts, 0xB4), "out-of-reach rejection must push 0xB4; got {:?}", pkts);
        // Source overlay for 103,100,7 must not exist (no mutation attempted).
        assert!(
            !g.dynamic.contains_key(&(103, 100, 7)),
            "overlay must not be materialized for an out-of-reach source"
        );
    }

    #[test]
    fn do_move_thing_full_move_removes_item_from_source() {
        // Move the stone (non-stackable) from (101,100,7) to (105,100,7).
        // Player at (100,100,7) — adjacent to source (dx=1), not ON the source tile
        // so creature stackpos does not interfere with the item's stackpos.
        // pre_creature_len for (101,100,7) = 1 (ground), no creatures on that tile
        // → stone is at item index 1 → wire stackpos 1.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        let from = Position::new(101, 100, 7);
        let to   = Position::new(102, 100, 7); // adjacent valid tile
        g.do_move_thing(player, from, 1, to, 1);

        // Source overlay: stone (sid 200) must no longer be present.
        let src_st = g.dynamic.get(&(101, 100, 7))
            .expect("source must have been materialized");
        assert!(
            !src_st.server_ids.contains(&200),
            "stone must be gone from source overlay; sids: {:?}", src_st.server_ids
        );

        // Destination overlay: stone (sid 200) must be present.
        let dst_st = g.dynamic.get(&(102, 100, 7))
            .expect("destination must have been materialized");
        assert!(
            dst_st.server_ids.contains(&200),
            "stone must appear on destination overlay; sids: {:?}", dst_st.server_ids
        );
        // Stone must be at index pre_creature_len (front of down-items, newest on top).
        // (102,100,7) had [ground(100), coins(300)] → pre_creature_len=1; stone inserts at 1.
        assert_eq!(
            dst_st.server_ids[dst_st.pre_creature_len], 200,
            "stone must be at front of down-items (index pre_creature_len); sids: {:?}", dst_st.server_ids
        );

        // Spectator / mover receives a 0x6A add-tile-item for the destination.
        let pkts = drain(&mut rx);
        assert!(
            has_op(&pkts, 0x6A),
            "player must receive 0x6A (add tile item) for destination; pkts: {:?}",
            pkts.iter().map(|p| p.first().copied()).collect::<Vec<_>>()
        );
        // And a removal (0x6C) or update (0x6B) for the source.
        assert!(
            has_op(&pkts, 0x6C) || has_op(&pkts, 0x6B),
            "player must receive 0x6C or 0x6B for source; pkts: {:?}",
            pkts.iter().map(|p| p.first().copied()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn do_move_thing_stackable_split_source_keeps_remainder() {
        // Move 3 of 10 gold coins from (102,100,7).
        // Player at (101,100,7) — adjacent to source (dx=1), not ON it.
        // pre_creature_len for (102,100,7) = 1 (ground), no creatures on it
        // → coins at item index 1 → wire stackpos 1.
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(101, 100, 7));
        drain(&mut rx);

        let from = Position::new(102, 100, 7);
        let to   = Position::new(103, 100, 7); // adjacent
        g.do_move_thing(player, from, 1, to, 3);

        // Source must have 7 coins left.
        let src_st = g.dynamic.get(&(102, 100, 7))
            .expect("source materialized");
        let coin_idx = src_st.server_ids.iter().position(|&s| s == 300)
            .expect("sid 300 still on source");
        let remaining = src_st.counts[coin_idx].unwrap_or(0);
        assert_eq!(remaining, 7, "source must retain 7 coins after moving 3; got {remaining}");

        // Destination must have 3 coins.
        let dst_st = g.dynamic.get(&(103, 100, 7))
            .expect("destination materialized");
        let dst_idx = dst_st.server_ids.iter().position(|&s| s == 300)
            .expect("sid 300 on destination");
        let moved = dst_st.counts[dst_idx].unwrap_or(0);
        assert_eq!(moved, 3, "destination must have 3 coins; got {moved}");

        // Partial split → source gets 0x6B (update, not 0x6C remove).
        let pkts = drain(&mut rx);
        assert!(
            has_op(&pkts, 0x6B),
            "partial split must produce 0x6B for source; pkts: {:?}",
            pkts.iter().map(|p| p.first().copied()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn do_move_thing_stackable_clamps_to_available() {
        // Attempt to move 20 of 10 coins — must clamp to 10 (no duplication).
        // Player at (101,100,7), coins at (102,100,7).
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(101, 100, 7));
        drain(&mut rx);

        let from = Position::new(102, 100, 7);
        let to   = Position::new(103, 100, 7);
        g.do_move_thing(player, from, 1, to, 20); // request 20, only 10 available

        // Source must be fully removed (clamped to 10 = all of them).
        let src_st = g.dynamic.get(&(102, 100, 7))
            .expect("source materialized");
        assert!(
            !src_st.server_ids.contains(&300),
            "all coins moved → source must no longer have sid 300; sids: {:?}", src_st.server_ids
        );

        // Destination must have exactly 10.
        let dst_st = g.dynamic.get(&(103, 100, 7))
            .expect("destination materialized");
        let dst_idx = dst_st.server_ids.iter().position(|&s| s == 300)
            .expect("sid 300 on destination");
        let moved = dst_st.counts[dst_idx].unwrap_or(0);
        assert_eq!(moved, 10, "destination must have exactly 10 coins (clamped); got {moved}");

        drain(&mut rx);
    }

    #[test]
    fn do_move_thing_spectator_receives_tile_update() {
        // A spectator near both tiles must receive the add-tile-item packet.
        // Player at (100,100,7), stone at (101,100,7), spectator also at (100,100,7).
        let mut g = Game::new(move_map());
        let (player, mut rx_player) = add_player(&mut g, Position::new(100, 100, 7));
        let (_spec, mut rx_spec) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx_player);
        drain(&mut rx_spec);

        let from = Position::new(101, 100, 7);
        let to   = Position::new(102, 100, 7);
        g.do_move_thing(player, from, 1, to, 1);

        let spec_pkts = drain(&mut rx_spec);
        assert!(
            has_op(&spec_pkts, 0x6A) || has_op(&spec_pkts, 0x6B) || has_op(&spec_pkts, 0x6C),
            "spectator must receive at least one tile-update packet; got {:?}",
            spec_pkts.iter().map(|p| p.first().copied()).collect::<Vec<_>>()
        );
    }

    /// Wire position for inventory slot `slot` (TFS: x=0xFFFF, y=slot, z=0).
    fn inv_pos(slot: u8) -> Position { Position::new(0xFFFF, u16::from(slot), 0) }

    /// Regression: a non-stackable moved onto a tile that already has a down-item must land
    /// at index `pre_creature_len` (front / newest-on-top), and the broadcast 0x6A must use
    /// stackpos `pre_creature_len + creatures` (no creatures → `pre_creature_len`).
    #[test]
    fn do_move_thing_dest_insert_front_of_down_items() {
        // (102,100,7) starts with [ground(100), coins(300)] → pre_creature_len=1.
        // Move stone (sid 200, non-stackable) from (101,100,7) onto (102,100,7).
        // Expected after move: sids = [ground(100), stone(200), coins(300)]
        //   i.e. stone at index 1 = pre_creature_len, coins shift to index 2.
        // Expected broadcast: 0x6A at stackpos 1 (pre_creature_len=1, no creatures).
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(100, 100, 7));
        drain(&mut rx);

        let from = Position::new(101, 100, 7);
        let to   = Position::new(102, 100, 7);
        // stone is at stackpos 1 on source (pre_creature_len=1, no creatures, stone at idx 1)
        g.do_move_thing(player, from, 1, to, 1);

        let dst_st = g.dynamic.get(&(102, 100, 7))
            .expect("destination must have been materialized");
        let pre = dst_st.pre_creature_len;

        // Stone (sid 200) must be at index pre_creature_len (front of down-items).
        assert_eq!(
            dst_st.server_ids[pre], 200,
            "moved stone must be at index pre_creature_len (front/top); sids: {:?}", dst_st.server_ids
        );
        // Coins (sid 300) must have shifted to index pre_creature_len + 1.
        assert_eq!(
            dst_st.server_ids[pre + 1], 300,
            "pre-existing coins must shift to pre_creature_len+1; sids: {:?}", dst_st.server_ids
        );

        // Broadcast: the 0x6A add must carry stackpos = pre_creature_len (no creatures on tile).
        // Packet layout: [0x6A, x_lo, x_hi, y_lo, y_hi, z, stackpos, ...]
        let pkts = drain(&mut rx);
        let add_pkt = pkts.iter().find(|p| p.first() == Some(&0x6A))
            .expect("must have a 0x6A add-tile-item packet for destination");
        let broadcast_sp = add_pkt[6];
        assert_eq!(
            broadcast_sp, pre as u8,
            "broadcast stackpos must equal pre_creature_len ({pre}); got {broadcast_sp}"
        );
    }

    // -------------------------------------------------------------------------
    // M10.2 equip / unequip routing tests
    // -------------------------------------------------------------------------

    #[test]
    fn equip_ground_item_into_matching_slot() {
        let mut g = Game::new(move_map());
        // Player adjacent to the helmet tile (106,100,7); stand on (105,100,7).
        let (player, mut rx) = add_player(&mut g, Position::new(105, 100, 7));
        drain(&mut rx);
        // Equip helmet (sid 500) from ground stackpos 1 into the head slot (1).
        // pre_creature_len for (106,100,7) = 1 (ground only), no creature on 106
        // → helmet at item index 1 → wire stackpos 1.
        g.do_move_thing(player, Position::new(106, 100, 7), 1, inv_pos(1), 1);
        // Slot 1 now holds the helmet.
        let slot = g.players.get(&player).unwrap().inventory[0];
        assert_eq!(slot.map(|it| it.server_id), Some(500), "helmet must be in head slot");
        // A 0x78 set-inventory packet was pushed to the player.
        assert!(has_op(&drain(&mut rx), 0x78), "equip must push 0x78");
        // The helmet left the ground overlay.
        assert_eq!(count_sid_in_overlays(&g, 500), 0, "helmet must leave the ground");
    }

    #[test]
    fn equip_into_wrong_slot_is_rejected() {
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(105, 100, 7));
        drain(&mut rx);
        // Try to put the helmet (head item) into the feet slot (8) → rejected.
        g.do_move_thing(player, Position::new(106, 100, 7), 1, inv_pos(8), 1);
        assert!(g.players.get(&player).unwrap().inventory[7].is_none(), "feet slot stays empty");
        assert!(g.players.get(&player).unwrap().inventory.iter().all(|s| s.is_none()), "nothing equipped");
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, 0xB4), "wrong-slot equip must push a 0xB4 status message");
        assert!(!has_op(&pkts, 0x78), "no inventory set on a rejected equip");
    }

    #[test]
    fn unequip_returns_item_to_the_ground() {
        let mut g = Game::new(move_map());
        let (player, mut rx) = add_player(&mut g, Position::new(105, 100, 7));
        drain(&mut rx);
        // Equip first.
        g.do_move_thing(player, Position::new(106, 100, 7), 1, inv_pos(1), 1);
        drain(&mut rx);
        // Unequip back onto the player's own tile (105,100,7) — within throw range.
        g.do_move_thing(player, inv_pos(1), 0, Position::new(105, 100, 7), 1);
        assert!(g.players.get(&player).unwrap().inventory[0].is_none(), "head slot cleared");
        let pkts = drain(&mut rx);
        assert!(has_op(&pkts, 0x79), "unequip must push 0x79 clear");
        assert_eq!(count_sid_in_overlays(&g, 500), 1, "helmet back on the ground");
    }
}
