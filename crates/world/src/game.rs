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

use protocol::chat::{self, SpeakType};
use protocol::creature::{self, CreatureView, Outfit};
use protocol::map_description::{PlacedCreature, TileSource};
use protocol::{enter_world, tile_creature, walk};

use crate::map::StaticMap;
use crate::{Direction, Position};

/// Outbound channel depth per session. A client that backs this up past the cap
/// is treated as dead (logged out) rather than blocking the game loop or growing
/// memory unbounded.
const PUSH_CAPACITY: usize = 256;

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
}

/// Login result: the new player's snapshot plus the already-in-range players,
/// pre-serialized as creature things to splice into the enter-world map.
pub struct LoginAck {
    pub snapshot: PlayerSnapshot,
    pub others: Vec<PlacedCreature>,
}

struct PlayerState {
    name: String,
    position: Position,
    direction: Direction,
    outfit: Outfit,
    push_tx: mpsc::Sender<Vec<u8>>,
    known: HashSet<u32>,
}

struct Game {
    map: Arc<StaticMap>,
    players: HashMap<u32, PlayerState>,
    next_id: u32,
    next_statement_id: u32,
}

impl Game {
    fn new(map: Arc<StaticMap>) -> Self {
        Game { map, players: HashMap::new(), next_id: 0x1000_0000, next_statement_id: 1 }
    }

    /// Can a viewer at `viewer` see tile `target`? The client renders an 18x14
    /// map description anchored at center-8 / center-6, so the viewport is
    /// ASYMMETRIC: it reaches one extra column east and one extra row south
    /// (dx in -8..=9, dy in -6..=7). Mirrors TFS `ProtocolGame::canSee`
    /// (`x <= myPos.x + maxClientViewportX + 1`). A symmetric `abs() <= 8` check
    /// is short by one tile on the east/south edge, which misaligns "creature
    /// entered view" from the slice that reveals it. Same floor; the multi-floor
    /// band is deferred.
    fn can_see(viewer: Position, target: Position) -> bool {
        let dx = i32::from(target.x) - i32::from(viewer.x);
        let dy = i32::from(target.y) - i32::from(viewer.y);
        viewer.z == target.z
            && (-VIEW_LEFT..=VIEW_RIGHT).contains(&dx)
            && (-VIEW_UP..=VIEW_DOWN).contains(&dy)
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
            Command::Login { name, outfit, push_tx, reply } => {
                let ack = self.login(name, outfit, push_tx);
                let _ = reply.send(ack);
            }
            Command::Logout { id } => self.logout(id),
            Command::Move { id, direction } => self.do_move(id, direction),
            Command::Turn { id, direction } => self.do_turn(id, direction),
            Command::Say { id, speak_type, text } => self.do_say(id, speak_type, text),
        }
    }

    /// Is a creature (other than `exclude`) standing on `pos`?
    fn tile_occupied(&self, pos: Position, exclude: u32) -> bool {
        self.players.iter().any(|(&pid, p)| pid != exclude && p.position == pos)
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

    fn login(&mut self, name: String, outfit: Outfit, push_tx: mpsc::Sender<Vec<u8>>) -> LoginAck {
        let id = self.next_id;
        self.next_id += 1;
        let position = self.free_spawn();
        let direction = Direction::South;

        // Existing in-range players, before inserting self.
        let others_ids = self.spectators(position, id);

        self.players.insert(id, PlayerState {
            name, position, direction, outfit, push_tx, known: HashSet::new(),
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
                let stackpos = self.map.creature_stackpos(
                    i32::from(position.x), i32::from(position.y), i32::from(position.z));
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

        LoginAck { snapshot: PlayerSnapshot { id, position, direction }, others }
    }

    fn logout(&mut self, id: u32) {
        let Some(p) = self.players.remove(&id) else { return };
        let pos = p.position;
        let stackpos = self.map.creature_stackpos(
            i32::from(pos.x), i32::from(pos.y), i32::from(pos.z));
        for spec in self.spectators(pos, id) {
            // A teleport puff on the departing creature's tile, then the remove.
            // (A deliberate polish over TFS, whose removeCreature disappears
            // silently; symmetric with the login appear effect.)
            self.push(spec, enter_world::magic_effect(
                pos.x, pos.y, pos.z, enter_world::EFFECT_TELEPORT));
            self.push(spec, tile_creature::remove_tile_thing((pos.x, pos.y, pos.z), stackpos));
            // The departed creature must be re-introduced (full form) if it ever
            // returns: drop it from each spectator's known-set.
            if let Some(s) = self.players.get_mut(&spec) { s.known.remove(&id); }
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
                for spec in self.spectators(pos, id) {
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
                for spec in self.spectators(pos, id) {
                    let Some(spos) = self.players.get(&spec).map(|p| p.position) else { continue };
                    let adjacent = (i32::from(spos.x) - i32::from(pos.x)).abs() <= 1
                        && (i32::from(spos.y) - i32::from(pos.y)).abs() <= 1;
                    let heard: &[u8] = if adjacent { &full } else { b"pspsps" };
                    self.push(spec, chat::creature_say(stmt, name.as_bytes(), LEVEL, speak_type, xyz, heard));
                }
            }
        }
    }

    fn do_move(&mut self, id: u32, direction: Direction) {
        let (from, cur_dir) = match self.players.get(&id) {
            Some(p) => (p.position, p.direction),
            None => return,
        };
        let (dx, dy) = direction.delta();
        let dest = from
            .offset(dx, dy)
            .filter(|&d| self.map.is_walkable(d) && !self.tile_occupied(d, id));

        let Some(to) = dest else {
            // Blocked: keep the original facing and snap the mover back;
            // spectators see nothing. Matches TFS: a failed walk never turns the
            // player (only Ctrl+arrows / 0x6F-0x72 do). cancel_walk carries the
            // unchanged direction so the client also keeps facing where it was.
            self.push(id, walk::cancel_walk(cur_dir.to_byte()));
            return;
        };
        // Successful move: now commit the new facing and position.
        if let Some(p) = self.players.get_mut(&id) { p.direction = direction; p.position = to; }

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
                self.push(spec, walk::creature_move(id, (to.x, to.y, to.z)));
            } else if sees_to {
                if let Some(bytes) = self.introduce(spec, id) {
                    let sp = self.map.creature_stackpos(
                        i32::from(to.x), i32::from(to.y), i32::from(to.z));
                    self.push(spec, tile_creature::add_tile_creature(
                        (to.x, to.y, to.z), sp, &bytes));
                }
            } else {
                // sees_from only: creature left this spectator's view.
                let sp = self.map.creature_stackpos(
                    i32::from(from.x), i32::from(from.y), i32::from(from.z));
                self.push(spec, tile_creature::remove_tile_thing((from.x, from.y, from.z), sp));
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
        for oid in left_view {
            if let Some(mover) = self.players.get_mut(&id) { mover.known.remove(&oid); }
        }

        // The mover's own view: 0x6D + revealed slices, carrying every other
        // player now in range so they render in the newly exposed tiles.
        let others_in_range: Vec<PlacedCreature> = self
            .visible_from(to, id)
            .into_iter()
            .filter_map(|oid| {
                let opos = self.players.get(&oid)?.position;
                let bytes = self.introduce(id, oid)?;
                Some(PlacedCreature { x: opos.x, y: opos.y, z: opos.z, bytes })
            })
            .collect();
        let pkt = walk::walk_update(
            id,
            (from.x, from.y, from.z),
            (to.x, to.y, to.z),
            self.map.as_ref(),
            &others_in_range,
        );
        self.push(id, pkt);
    }
}

enum Command {
    Login { name: String, outfit: Outfit, push_tx: mpsc::Sender<Vec<u8>>, reply: oneshot::Sender<LoginAck> },
    Logout { id: u32 },
    Move { id: u32, direction: Direction },
    Turn { id: u32, direction: Direction },
    Say { id: u32, speak_type: SpeakType, text: String },
}

/// Cloneable handle to the running world.
#[derive(Clone)]
pub struct WorldHandle {
    tx: mpsc::Sender<Command>,
    pub map: Arc<StaticMap>,
}

impl WorldHandle {
    /// Register a player. The caller supplies the session's outbound channel and
    /// the player's outfit. Returns the snapshot + in-range players to render.
    pub async fn login(
        &self,
        name: String,
        outfit: Outfit,
        push_tx: mpsc::Sender<Vec<u8>>,
    ) -> Option<LoginAck> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Command::Login { name, outfit, push_tx, reply }).await.ok()?;
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
}

/// The outbound channel a session hands the world at login.
pub fn push_channel() -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    mpsc::channel(PUSH_CAPACITY)
}

/// Spawn the world actor task and return a handle.
pub fn spawn(map: Arc<StaticMap>) -> WorldHandle {
    let (tx, mut rx) = mpsc::channel::<Command>(64);
    let handle = WorldHandle { tx, map: Arc::clone(&map) };
    tokio::spawn(async move {
        let mut game = Game::new(map);
        while let Some(cmd) = rx.recv().await {
            game.handle(cmd);
        }
    });
    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::StaticMap;
    use formats::otb::{ItemType, ItemsOtb};
    use formats::otbm::{MapItem, MapTile, OtbmMap, Town};

    fn walk_map() -> Arc<StaticMap> {
        let items = ItemsOtb {
            major_version: 3, minor_version: 57, build_number: 0,
            items: vec![
                ItemType { group: 1, flags: 0, server_id: 100, client_id: 4526, always_on_top: false, top_order: 0 },
                ItemType { group: 5, flags: 0x0000_0001, server_id: 200, client_id: 1059, always_on_top: false, top_order: 0 },
            ],
        };
        let ground = |x, y| MapTile { x, y, z: 7, flags: 0, house_id: None,
            items: vec![MapItem { id: 100, contents: vec![] }] };
        let map = OtbmMap {
            width: 200, height: 200, major_items: 3, minor_items: 57,
            description: String::new(), spawn_file: None, house_file: None,
            tiles: vec![
                ground(95, 117), ground(96, 117), ground(95, 116),
                // wall to the west of spawn
                MapTile { x: 94, y: 117, z: 7, flags: 0, house_id: None,
                    items: vec![MapItem { id: 100, contents: vec![] }, MapItem { id: 200, contents: vec![] }] },
            ],
            towns: vec![Town { id: 1, name: "Thais".into(), x: 95, y: 117, z: 7 }],
            waypoints: vec![],
        };
        Arc::new(StaticMap::from_formats(&map, &items))
    }

    fn knight() -> Outfit {
        Outfit { look_type: 128, head: 78, body: 69, legs: 58, feet: 76, addons: 0, mount: 0 }
    }

    /// Insert a player at `pos` and return (id, its push receiver).
    fn add_player(g: &mut Game, pos: Position) -> (u32, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(super::PUSH_CAPACITY);
        let id = g.next_id;
        g.next_id += 1;
        g.players.insert(id, PlayerState {
            name: "Tester".into(), position: pos, direction: Direction::South,
            outfit: knight(), push_tx: tx, known: HashSet::new(),
        });
        (id, rx)
    }

    #[test]
    fn spectators_within_client_viewport_same_floor() {
        let mut g = Game::new(walk_map());
        let (a, _ra) = add_player(&mut g, Position::new(100, 100, 7));
        let (b, _rb) = add_player(&mut g, Position::new(108, 106, 7)); // edge: +8x +6y
        let (c, _rc) = add_player(&mut g, Position::new(109, 100, 7)); // +9x out
        let (d, _rd) = add_player(&mut g, Position::new(100, 100, 6)); // other floor
        let specs = g.spectators(Position::new(100, 100, 7), a);
        assert!(specs.contains(&b), "edge of viewport is visible");
        assert!(!specs.contains(&c), "beyond +8x is not visible");
        assert!(!specs.contains(&d), "other floor is not visible");
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
        let world = spawn(walk_map());
        let (tx_a, mut rx_a) = push_channel();
        let ack_a = world.login("A".into(), knight(), tx_a).await.unwrap();
        // Second player logs in next to A; A must receive a 0x6A appear.
        let (tx_b, _rx_b) = push_channel();
        let _ack_b = world.login("B".into(), knight(), tx_b).await.unwrap();
        let pkt = rx_a.recv().await.unwrap();
        assert_eq!(pkt[0], protocol::tile_creature::OP_ADD_TILE_CREATURE);
        // ...followed by the teleport puff, so spectators see the spawn effect too.
        let effect = rx_a.recv().await.unwrap();
        assert_eq!(effect[0], protocol::enter_world::OP_MAGIC_EFFECT);
        assert_ne!(ack_a.snapshot.id, 0);
    }

    #[tokio::test]
    async fn second_login_sees_first_in_ack_others() {
        let world = spawn(walk_map());
        let (tx_a, _rx_a) = push_channel();
        world.login("A".into(), knight(), tx_a).await.unwrap();
        let (tx_b, _rx_b) = push_channel();
        let ack_b = world.login("B".into(), knight(), tx_b).await.unwrap();
        assert_eq!(ack_b.others.len(), 1, "B's enter-world includes A");
    }

    #[tokio::test]
    async fn move_pushes_creature_move_to_spectator() {
        let world = spawn(walk_map());
        let (tx_a, mut rx_a) = push_channel();
        let ack_a = world.login("A".into(), knight(), tx_a).await.unwrap();
        let (tx_b, mut rx_b) = push_channel();
        let _ack_b = world.login("B".into(), knight(), tx_b).await.unwrap();
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
        let world = spawn(walk_map());
        let (tx_a, mut rx_a) = push_channel();
        world.login("A".into(), knight(), tx_a).await.unwrap();
        let (tx_b, _rx_b) = push_channel();
        let ack_b = world.login("B".into(), knight(), tx_b).await.unwrap();
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
        let world = spawn(walk_map());
        let (tx_a, _ra) = push_channel();
        let ack_a = world.login("A".into(), knight(), tx_a).await.unwrap();
        let (tx_b, _rb) = push_channel();
        let ack_b = world.login("B".into(), knight(), tx_b).await.unwrap();
        assert_ne!(
            ack_a.snapshot.position,
            ack_b.snapshot.position,
            "co-logins must not share a tile"
        );
    }

    #[tokio::test]
    async fn say_broadcasts_to_spectator_and_speaker() {
        let world = spawn(walk_map());
        let (tx_a, mut rx_a) = push_channel();
        let ack_a = world.login("A".into(), knight(), tx_a).await.unwrap();
        let (tx_b, mut rx_b) = push_channel();
        world.login("B".into(), knight(), tx_b).await.unwrap();
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
}
